use golutra_auth::{CredentialSource, OpenAiDeviceAuthorizationDescriptor};
use golutra_config::ProviderSettings;
use golutra_llm::ProviderReasoningEffort;
use golutra_protocol::{RuntimeEventType, VisibleStep};
use ratatui::backend::TestBackend;
use ratatui::layout::{Position, Rect};
use ratatui::style::Color;
use ratatui::text::Line;
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
    )
    .with_transport_runtime_controls(&transport);
    assert!(app.runtime_controls.profile_name.is_none());
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

#[tokio::test]
async fn vim_mode_edits_unicode_and_preserves_insert_normal_boundaries() {
    let transport = RuntimeTransport::in_memory().await.expect("transport");
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.preferences.keymap = KeymapMode::Vim;
    app.composer_mode = ComposerMode::VimInsert;
    handle_paste("你👍 alpha\n第二 line", &mut app);

    handle_key(
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .expect("enter normal mode");
    assert_eq!(app.composer_mode, ComposerMode::VimNormal);

    handle_paste("blocked", &mut app);
    assert_eq!(app.input.text(), "你👍 alpha\n第二 line");
    assert_eq!(app.status_message, "enter Vim insert mode before pasting");

    for key in [KeyCode::Char('0'), KeyCode::Char('x')] {
        handle_key(KeyEvent::new(key, KeyModifiers::NONE), &mut app, &transport)
            .await
            .expect("normal-mode edit");
    }
    assert_eq!(app.input.text(), "你👍 alpha\n二 line");

    handle_key(
        KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .expect("undo character deletion");
    assert_eq!(app.input.text(), "你👍 alpha\n第二 line");

    for key in [KeyCode::Char('d'), KeyCode::Char('d')] {
        handle_key(KeyEvent::new(key, KeyModifiers::NONE), &mut app, &transport)
            .await
            .expect("delete current line");
    }
    assert_eq!(app.input.text(), "你👍 alpha");

    handle_key(
        KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .expect("undo line deletion");
    handle_key(
        KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT),
        &mut app,
        &transport,
    )
    .await
    .expect("append at line end");
    assert_eq!(app.composer_mode, ComposerMode::VimInsert);
    handle_key(
        KeyEvent::new(KeyCode::Char('界'), KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .expect("insert unicode in Vim mode");
    assert_eq!(app.input.text(), "你👍 alpha\n第二 line界");
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

#[tokio::test]
async fn structured_question_free_text_uses_unicode_composer_editing() {
    let transport = RuntimeTransport::in_memory().await.expect("transport");
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.question_dialog = Some(QuestionDialogState::new(UserQuestionRequest {
        question_id: golutra_core::QuestionId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        tool_call_id: golutra_core::ToolCallId::new(),
        questions: vec![golutra_core::UserQuestionPrompt {
            id: "format".to_owned(),
            header: "Output".to_owned(),
            question: "Choose an output format".to_owned(),
            mode: golutra_core::UserQuestionMode::Single,
            options: vec![
                golutra_core::UserQuestionOption {
                    id: "json".to_owned(),
                    label: "JSON".to_owned(),
                    description: None,
                },
                golutra_core::UserQuestionOption {
                    id: "text".to_owned(),
                    label: "Text".to_owned(),
                    description: None,
                },
            ],
        }],
    }));

    handle_key(
        KeyEvent::new(KeyCode::Char('自'), KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .expect("type free text");
    handle_paste("定义👍🏽", &mut app);
    handle_key(
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .expect("delete emoji grapheme");
    handle_paste("\r\n说明", &mut app);

    let dialog = app.question_dialog.as_ref().expect("question dialog");
    assert!(dialog.is_free_text_focused());
    assert_eq!(dialog.current_free_text().text(), "自定义\n说明");
    let resolution = dialog.resolution("test").expect("free text answer");
    assert!(resolution.answers[0].selected_option_ids.is_empty());
    assert_eq!(
        resolution.answers[0].free_text.as_deref(),
        Some("自定义\n说明")
    );
}

#[test]
fn terminal_resume_generation_invalidates_the_crossterm_input_stream() {
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    let generation = app.terminal_resume_generation;
    assert!(!app.terminal_input_stream_is_stale(generation));

    app.mark_terminal_resumed();

    assert!(app.terminal_input_stream_is_stale(generation));
}

#[test]
fn session_history_card_uses_golutra_brand_and_operational_fields() {
    let app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    )
    .with_yolo(true)
    .with_footer_context("/workspace", "gpt-test");

    let lines = session_history_lines(&app, 120);
    let text = lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let mut logo_colors = Vec::new();
    for color in lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .filter(|span| span.content.contains('█'))
        .filter_map(|span| span.style.fg)
    {
        if !logo_colors.contains(&color) {
            logo_colors.push(color);
        }
    }

    assert!(text.contains("GOLUTRA"));
    assert!(!text.contains("plan > act > verify"));
    assert!(text.contains("engine"));
    assert!(text.contains("gpt-test"));
    assert!(text.contains("scope"));
    assert!(text.contains("/workspace"));
    assert!(text.contains("guard"));
    assert!(text.contains("unrestricted"));
    assert!(text.contains("██║  ███╗"));
    assert!(text.contains("╚══════╝"));
    assert!(logo_colors.len() >= 12);
    assert!(
        logo_colors
            .iter()
            .all(|color| matches!(color, Color::Rgb(_, _, _)))
    );
    assert!(!text.contains("new in"));
    assert!(!text.contains("F1 help"));
    assert!(!text.contains("/settings"));
}

#[test]
fn session_history_logo_is_responsive_and_screen_reader_safe() {
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    )
    .with_footer_context("/workspace", "gpt-test");

    for width in [7, 8, 15, 24, 60, 112, 113, 120, 160] {
        assert!(
            session_history_lines(&app, width)
                .iter()
                .all(|line| line.width() <= usize::from(width)),
            "width {width}"
        );
    }
    assert!(
        !session_history_lines(&app, 112)
            .iter()
            .any(|line| line.to_string().contains('█'))
    );
    assert!(
        session_history_lines(&app, 113)
            .iter()
            .any(|line| line.to_string().contains('█'))
    );

    app.preferences.theme = ColorTheme::Monochrome;
    let monochrome = session_history_lines(&app, 160);
    assert!(
        monochrome
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| span.content.contains('█'))
            .all(|span| span.style.fg == Some(Color::White))
    );

    app.preferences.theme = ColorTheme::Classic;
    app.preferences.high_contrast = true;
    let high_contrast = session_history_lines(&app, 160);
    assert!(
        high_contrast
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| span.content.contains('█'))
            .all(|span| span.style.fg == Some(Color::LightCyan))
    );

    app.preferences.screen_reader = true;
    let accessible = session_history_lines(&app, 120)
        .iter()
        .map(ToString::to_string)
        .collect::<String>();
    assert!(accessible.contains("GOLUTRA"));
    assert!(!accessible.contains('█'));
}

#[test]
fn inline_session_history_is_inserted_only_once() {
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    )
    .with_footer_context("/workspace", "gpt-test");
    app.enable_inline_history();
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(12),
        },
    )
    .expect("inline terminal");
    let mut history = InlineHistoryState::new(app.session_id);

    assert!(history.flush(&mut terminal, &mut app).expect("first flush"));
    assert!(
        !history
            .flush(&mut terminal, &mut app)
            .expect("second flush")
    );

    assert_eq!(
        terminal_buffer_text(&terminal).matches("GOLUTRA").count(),
        1
    );

    app.request_history_rebuild();
    assert!(history.flush(&mut terminal, &mut app).expect("rebuild"));
    assert_eq!(
        terminal_buffer_text(&terminal).matches("GOLUTRA").count(),
        1
    );
}

#[test]
fn completed_history_keeps_latest_response_next_to_composer() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        Some(task_id),
        false,
        "ready (mock)".to_owned(),
        None,
    )
    .with_footer_context("/workspace", "gpt-test");
    app.events = vec![
        transcript_event(
            1,
            session_id,
            task_id,
            RuntimeEventType::TaskCreated,
            json!({"payload": {"prompt": "compare completed layout"}}),
        ),
        transcript_event(
            2,
            session_id,
            task_id,
            RuntimeEventType::AssistantMessage,
            json!({"content": "latest completed response"}),
        ),
    ];
    app.projection = Some(UserProjection {
        session_id,
        task_id: Some(task_id),
        status: golutra_core::TaskStatus::Completed,
        visible_steps: Vec::new(),
        pending_approval: None,
        final_message: None,
        residual_risks: Vec::new(),
    });
    app.enable_inline_history();
    let mut terminal = Terminal::with_options(
        TestBackend::new(80, 24),
        TerminalOptions {
            viewport: Viewport::Inline(12),
        },
    )
    .expect("inline terminal");
    let mut history = InlineHistoryState::new(session_id);

    history
        .flush(&mut terminal, &mut app)
        .expect("completed history");
    terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("draw completed frame");

    let rows = (0..24)
        .map(|row| {
            (0..80)
                .filter_map(|column| terminal.backend().buffer().cell((column, row)))
                .map(|cell| cell.symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    let response_row = rows
        .iter()
        .position(|row| row.contains("latest completed response"))
        .expect("latest response row");
    let composer_row = rows
        .iter()
        .position(|row| row.contains("Ask Golutra to change code or inspect the workspace"))
        .expect("composer row");

    assert!(
        composer_row.saturating_sub(response_row) <= 3,
        "latest response must stay immediately above the composer: {rows:#?}"
    );
}

#[test]
fn resumed_turns_remain_contiguous_above_the_composer() {
    let session_id = SessionId::new();
    let first_task = TaskId::new();
    let second_task = TaskId::new();
    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        Some(second_task),
        false,
        "ready (mock)".to_owned(),
        None,
    )
    .with_footer_context("/workspace", "gpt-test");
    let mut events = vec![
        transcript_event(
            1,
            session_id,
            first_task,
            RuntimeEventType::TaskCreated,
            json!({"payload": {"prompt": "first restored prompt"}}),
        ),
        transcript_event(
            2,
            session_id,
            first_task,
            RuntimeEventType::AssistantMessage,
            json!({"content": "first restored answer"}),
        ),
    ];
    events.extend((3..=14).map(|sequence_no| {
        transcript_event(
            sequence_no,
            session_id,
            first_task,
            RuntimeEventType::AssistantMessage,
            json!({"content": format!("archived answer {sequence_no}")}),
        )
    }));
    events.extend([
        transcript_event(
            15,
            session_id,
            second_task,
            RuntimeEventType::TaskCreated,
            json!({"payload": {"prompt": "second restored prompt"}}),
        ),
        transcript_event(
            16,
            session_id,
            second_task,
            RuntimeEventType::AssistantMessage,
            json!({"content": "second restored answer"}),
        ),
    ]);
    app.events = events;
    app.projection = Some(UserProjection {
        session_id,
        task_id: Some(second_task),
        status: golutra_core::TaskStatus::Completed,
        visible_steps: Vec::new(),
        pending_approval: None,
        final_message: None,
        residual_risks: Vec::new(),
    });
    app.enable_inline_history();
    let mut terminal = Terminal::with_options(
        TestBackend::new(80, 40),
        TerminalOptions {
            viewport: Viewport::Inline(24),
        },
    )
    .expect("inline terminal");
    let mut history = InlineHistoryState::new(session_id);

    history
        .flush(&mut terminal, &mut app)
        .expect("restored history");
    assert!(
        !app.inline_history_committed_event_ids.is_empty(),
        "fixture must cross the scrollback/live boundary"
    );
    draw_inline_test_frame(&mut terminal, &mut app);

    let rows = terminal_buffer_rows(&terminal);
    let prompt_row = rows
        .iter()
        .position(|row| row.contains("second restored prompt"))
        .expect("second prompt row");
    let answer_row = rows
        .iter()
        .position(|row| row.contains("second restored answer"))
        .expect("second answer row");
    assert!(
        answer_row.saturating_sub(prompt_row) <= 2,
        "restored messages must not be separated by viewport padding: {rows:#?}"
    );
}

#[test]
fn resumed_debug_events_remain_contiguous_above_the_composer() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let events = (1..=30)
        .map(|sequence_no| {
            transcript_event(
                sequence_no,
                session_id,
                task_id,
                RuntimeEventType::AssistantMessage,
                json!({"content": format!("debug event {sequence_no}")}),
            )
        })
        .collect::<Vec<_>>();
    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        Some(task_id),
        true,
        "ready (mock)".to_owned(),
        None,
    )
    .with_footer_context("/workspace", "gpt-test");
    app.events = events.clone();
    app.developer_projection = Some(debug_projection_with_events(
        session_id,
        Some(task_id),
        events,
    ));
    app.enable_inline_history();
    let mut terminal = Terminal::with_options(
        TestBackend::new(80, 40),
        TerminalOptions {
            viewport: Viewport::Inline(24),
        },
    )
    .expect("inline terminal");
    let mut history = InlineHistoryState::new(session_id);

    history
        .flush(&mut terminal, &mut app)
        .expect("restored debug history");
    assert!(
        !app.inline_history_committed_event_ids.is_empty(),
        "fixture must cross the scrollback/live boundary"
    );
    draw_inline_test_frame(&mut terminal, &mut app);

    let rows = terminal_buffer_rows(&terminal);
    let previous_row = rows
        .iter()
        .position(|row| row.contains("#29 AssistantMessage/Runtime"))
        .expect("previous debug event row");
    let latest_row = rows
        .iter()
        .position(|row| row.contains("#30 AssistantMessage/Runtime"))
        .expect("latest debug event row");
    let rows_between = &rows[previous_row.saturating_add(1)..latest_row];
    assert!(
        rows_between
            .iter()
            .filter(|row| row.trim().is_empty())
            .count()
            <= 1,
        "debug events must not be separated by viewport padding: {rows:#?}"
    );
}

#[test]
fn debug_history_sequences_transcript_before_observations_in_terminal_scrollback() {
    for width in [80, 120, 160] {
        let session_id = SessionId::new();
        let task_id = TaskId::new();
        let events = (1..=36)
            .map(|sequence_no| {
                transcript_event(
                    sequence_no,
                    session_id,
                    task_id,
                    RuntimeEventType::AssistantMessage,
                    json!({"content": format!("paired 正文 {sequence_no}")}),
                )
            })
            .collect::<Vec<_>>();
        let mut app = TuiApp::new(
            ThreadId::new(),
            session_id,
            Some(task_id),
            true,
            "ready (mock)".to_owned(),
            None,
        )
        .with_footer_context("/workspace", "gpt-test");
        app.events = events.clone();
        app.developer_projection = Some(debug_projection_with_events(
            session_id,
            Some(task_id),
            events,
        ));
        app.enable_inline_history();
        let mut terminal = Terminal::with_options(
            TestBackend::new(width, 320),
            TerminalOptions {
                viewport: Viewport::Inline(12),
            },
        )
        .expect("inline terminal");
        let mut history = InlineHistoryState::new(session_id);

        history
            .flush(&mut terminal, &mut app)
            .expect("paired debug history");
        draw_inline_test_frame(&mut terminal, &mut app);
        let rows = terminal_buffer_display_rows(&terminal);
        let transcript_row = rows
            .iter()
            .position(|row| row.contains("paired 正文 1"))
            .expect("oldest transcript in terminal scrollback");
        let observation_row = rows
            .iter()
            .position(|row| row.contains("#1 AssistantMessage/Runtime"))
            .expect("oldest observation in terminal scrollback");
        assert!(
            transcript_row < observation_row,
            "{width}: transcript must precede its observation: {rows:#?}"
        );
        assert!(
            !rows[transcript_row].contains("#1 AssistantMessage/Runtime"),
            "{width}: {:?}",
            rows[transcript_row]
        );
        assert!(
            !rows[observation_row].contains("paired 正文 1"),
            "{width}: {:?}",
            rows[observation_row]
        );
        let first = &rows[observation_row];
        let observation_column = first.find("#1 ").expect("observation column");
        let (transcript_width, _) = debug_pane_widths(width);
        assert_eq!(
            display_width(&first[..observation_column]),
            usize::from(transcript_width),
            "{width}: {first:?}"
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.contains("#1 AssistantMessage/Runtime"))
                .count(),
            1,
            "{width}: observation history must not be duplicated"
        );

        let facts = rows
            .iter()
            .find(|row| row.contains("facts events=36"))
            .expect("expanded facts in terminal scrollback");
        let facts_column = facts.find("facts events=36").expect("facts column");
        assert_eq!(
            display_width(&facts[..facts_column]),
            usize::from(transcript_width),
            "{width}: {facts:?}"
        );
    }
}

#[test]
fn debug_split_history_keeps_both_columns_inside_equal_halves() {
    for width in [80, 81, 120, 121] {
        let (transcript_width, developer_width) = debug_pane_widths(width);
        assert_eq!(transcript_width, width / 2);
        assert_eq!(developer_width, width - transcript_width);
        assert!(transcript_width.abs_diff(developer_width) <= 1);

        let rows = debug_split_history_lines(
            vec![Line::from("L".repeat(usize::from(width) * 2))],
            vec![Line::from("R".repeat(usize::from(width) * 2))],
            width,
        );
        assert!(!rows.is_empty());
        let mut saw_transcript = false;
        let mut saw_observation = false;
        for row in rows {
            let text = row.to_string();
            assert_eq!(
                display_width(&text),
                usize::from(width),
                "{width}: {text:?}"
            );
            let boundary = usize::from(transcript_width);
            let transcript_has_content = text[..boundary].contains('L');
            let observation_has_content = text[boundary..].contains('R');
            assert_ne!(
                transcript_has_content, observation_has_content,
                "each physical debug row must belong to exactly one side at width {width}: {text:?}"
            );
            saw_transcript |= transcript_has_content;
            saw_observation |= observation_has_content;
            assert!(
                text[..boundary]
                    .chars()
                    .all(|character| matches!(character, 'L' | ' ')),
                "left transcript crossed its half at width {width}: {text:?}"
            );
            assert!(
                text[boundary..]
                    .chars()
                    .all(|character| matches!(character, 'R' | ' ')),
                "right observation crossed its half at width {width}: {text:?}"
            );
        }
        assert!(saw_transcript && saw_observation);

        let unicode_rows = debug_split_history_lines(
            vec![Line::from("旅途正文".repeat(usize::from(width)))],
            vec![Line::from("运行观测".repeat(usize::from(width)))],
            width,
        );
        assert!(
            unicode_rows
                .iter()
                .all(|row| row.width() == usize::from(width))
        );
    }
}

#[test]
fn large_debug_history_does_not_exceed_ratatui_buffer_area() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let events = (1..=1_000)
        .map(|sequence_no| {
            transcript_event(
                sequence_no,
                session_id,
                task_id,
                RuntimeEventType::AssistantMessage,
                json!({"content": format!("debug event {sequence_no}")}),
            )
        })
        .collect::<Vec<_>>();
    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        Some(task_id),
        true,
        "ready (mock)".to_owned(),
        None,
    )
    .with_footer_context("/workspace", "gpt-test");
    app.events = events.clone();
    app.developer_projection = Some(debug_projection_with_events(
        session_id,
        Some(task_id),
        events,
    ));
    app.enable_inline_history();
    let mut terminal = Terminal::with_options(
        TestBackend::new(80, 40),
        TerminalOptions {
            viewport: Viewport::Inline(12),
        },
    )
    .expect("inline terminal");
    let mut history = InlineHistoryState::new(session_id);

    assert!(
        history
            .flush(&mut terminal, &mut app)
            .expect("large debug history")
    );
    draw_inline_test_frame(&mut terminal, &mut app);
    let rendered = terminal_buffer_text(&terminal);
    let committed = rendered.find("#999 ").expect("last committed debug event");
    let live = rendered.find("#1000 ").expect("live debug tail");
    assert!(
        committed < live,
        "debug history batches must preserve order"
    );
}

#[test]
fn session_switch_clears_previous_history_while_replay_is_loading() {
    let first_session = SessionId::new();
    let first_task = TaskId::new();
    let mut app = TuiApp::new(
        ThreadId::new(),
        first_session,
        Some(first_task),
        false,
        "ready (mock)".to_owned(),
        None,
    )
    .with_footer_context("/workspace", "gpt-test");
    app.events = vec![transcript_event(
        1,
        first_session,
        first_task,
        RuntimeEventType::AssistantMessage,
        json!({"content": "old-session-only"}),
    )];
    app.enable_inline_history();
    let mut terminal = Terminal::with_options(
        TestBackend::new(80, 40),
        TerminalOptions {
            viewport: Viewport::Inline(12),
        },
    )
    .expect("inline terminal");
    let mut history = InlineHistoryState::new(first_session);

    history
        .flush(&mut terminal, &mut app)
        .expect("initial history");
    draw_inline_test_frame(&mut terminal, &mut app);
    assert!(terminal_buffer_text(&terminal).contains("old-session-only"));

    let next_session = SessionId::new();
    let next_task = TaskId::new();
    app.session_id = next_session;
    app.task_id = Some(next_task);
    app.begin_history_replay();
    app.events.clear();

    assert!(
        history
            .flush(&mut terminal, &mut app)
            .expect("clear old history")
    );
    assert!(!terminal_buffer_text(&terminal).contains("old-session-only"));
    assert!(
        !history
            .flush(&mut terminal, &mut app)
            .expect("loading redraw")
    );

    app.events = vec![transcript_event(
        1,
        next_session,
        next_task,
        RuntimeEventType::AssistantMessage,
        json!({"content": "new-session-only"}),
    )];
    app.invalidate_transcript_layout();
    app.history_replay_ready = true;
    assert!(
        history
            .flush(&mut terminal, &mut app)
            .expect("replayed history")
    );
    draw_inline_test_frame(&mut terminal, &mut app);
    let replayed = terminal_buffer_text(&terminal);
    assert!(replayed.contains("new-session-only"));
    assert!(!replayed.contains("old-session-only"));
}

#[test]
fn debug_switch_keeps_transcript_visible_before_projection_finishes() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let events = vec![transcript_event(
        1,
        session_id,
        task_id,
        RuntimeEventType::AssistantMessage,
        json!({"content": "transcript-projection-only"}),
    )];
    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        Some(task_id),
        false,
        "ready (mock)".to_owned(),
        None,
    )
    .with_footer_context("/workspace", "gpt-test");
    app.events = events.clone();
    app.enable_inline_history();
    let mut terminal = Terminal::with_options(
        TestBackend::new(80, 40),
        TerminalOptions {
            viewport: Viewport::Inline(12),
        },
    )
    .expect("inline terminal");
    let mut history = InlineHistoryState::new(session_id);

    history
        .flush(&mut terminal, &mut app)
        .expect("initial transcript");
    app.set_debug_mode(true);
    assert!(app.developer_projection.is_none());

    assert!(
        history
            .flush(&mut terminal, &mut app)
            .expect("rebuild transcript")
    );
    draw_inline_test_frame(&mut terminal, &mut app);
    assert!(terminal_buffer_text(&terminal).contains("transcript-projection-only"));
    assert!(
        !history
            .flush(&mut terminal, &mut app)
            .expect("debug loading redraw")
    );

    app.developer_error = Some("debug projection unavailable".to_owned());
    assert!(
        history
            .flush(&mut terminal, &mut app)
            .expect("failed debug projection")
    );
    draw_inline_test_frame(&mut terminal, &mut app);
    let failed = terminal_buffer_text(&terminal);
    assert_eq!(failed.matches("transcript-projection-only").count(), 1);
    assert!(failed.contains("debug projection unavailable"));
}

#[test]
fn debug_scrollback_waits_for_canonical_history_before_committing_projection_events() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let events = (1..=36)
        .map(|sequence_no| {
            transcript_event(
                sequence_no,
                session_id,
                task_id,
                RuntimeEventType::AssistantMessage,
                json!({"content": format!("canonical transcript {sequence_no}")}),
            )
        })
        .collect::<Vec<_>>();
    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        Some(task_id),
        true,
        "ready (mock)".to_owned(),
        None,
    )
    .with_footer_context("/workspace", "gpt-test");
    app.developer_projection = Some(debug_projection_with_events(
        session_id,
        Some(task_id),
        events.clone(),
    ));
    app.enable_inline_history();
    let mut terminal = Terminal::with_options(
        TestBackend::new(80, 240),
        TerminalOptions {
            viewport: Viewport::Inline(12),
        },
    )
    .expect("inline terminal");
    let mut history = InlineHistoryState::new(session_id);

    history
        .flush(&mut terminal, &mut app)
        .expect("projection-only debug frame");
    assert!(app.inline_history_committed_event_ids.is_empty());
    draw_inline_test_frame(&mut terminal, &mut app);
    assert!(terminal_buffer_text(&terminal).contains("#36 AssistantMessage/Runtime"));

    app.events = events;
    app.invalidate_transcript_layout();
    history
        .flush(&mut terminal, &mut app)
        .expect("canonical debug history");
    assert!(!app.inline_history_committed_event_ids.is_empty());
    draw_inline_test_frame(&mut terminal, &mut app);
    assert!(terminal_buffer_text(&terminal).contains("canonical transcript 36"));
}

#[test]
fn repeated_debug_and_transcript_switches_do_not_duplicate_history() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let events = vec![
        transcript_event(
            1,
            session_id,
            task_id,
            RuntimeEventType::TaskCreated,
            json!({"payload": {"prompt": "unique-replay-prompt"}}),
        ),
        transcript_event(
            2,
            session_id,
            task_id,
            RuntimeEventType::AssistantMessage,
            json!({"content": "unique-replay-answer"}),
        ),
    ];
    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        Some(task_id),
        false,
        "ready (mock)".to_owned(),
        None,
    )
    .with_footer_context("/workspace", "gpt-test");
    app.events = events.clone();
    app.developer_projection = Some(debug_projection_with_events(
        session_id,
        Some(task_id),
        events,
    ));
    app.enable_inline_history();
    let mut terminal = Terminal::with_options(
        TestBackend::new(80, 80),
        TerminalOptions {
            viewport: Viewport::Inline(12),
        },
    )
    .expect("inline terminal");
    let mut history = InlineHistoryState::new(session_id);

    history.flush(&mut terminal, &mut app).expect("transcript");
    draw_inline_test_frame(&mut terminal, &mut app);
    assert_eq!(
        terminal_buffer_text(&terminal)
            .matches("unique-replay-prompt")
            .count(),
        1
    );

    app.set_debug_mode(true);
    history.flush(&mut terminal, &mut app).expect("debug");
    draw_inline_test_frame(&mut terminal, &mut app);
    let debug = terminal_buffer_text(&terminal);
    assert_eq!(debug.matches("#1 TaskCreated/Runtime").count(), 1);
    assert_eq!(debug.matches("#2 AssistantMessage/Runtime").count(), 1);

    app.toggle_transcript_fullscreen();
    history
        .flush(&mut terminal, &mut app)
        .expect("transcript switch");
    draw_inline_test_frame(&mut terminal, &mut app);
    let transcript = terminal_buffer_text(&terminal);
    assert_eq!(transcript.matches("unique-replay-prompt").count(), 1);
    assert!(!transcript.contains("#1 TaskCreated/Runtime"));

    app.toggle_transcript_fullscreen();
    history
        .flush(&mut terminal, &mut app)
        .expect("debug switch");
    draw_inline_test_frame(&mut terminal, &mut app);
    let debug_again = terminal_buffer_text(&terminal);
    assert_eq!(debug_again.matches("#1 TaskCreated/Runtime").count(), 1);
    assert_eq!(debug_again.matches("GOLUTRA").count(), 1);
}

#[test]
fn session_history_card_fits_narrow_unicode_paths() {
    let app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    )
    .with_footer_context("/工作区/非常长的目录名称/project", "模型-very-long-name");

    let lines = session_history_lines(&app, 24);
    let text = lines.iter().map(ToString::to_string).collect::<String>();

    assert!(lines.iter().all(|line| line.width() <= 24));
    assert!(lines.iter().any(|line| line.to_string().contains('…')));
    assert!(text.contains("GOLUTRA"));
    assert!(!text.contains('█'));
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
fn structured_question_free_text_owns_focus_and_renders_a_unicode_cursor() {
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    let mut dialog = QuestionDialogState::new(UserQuestionRequest {
        question_id: golutra_core::QuestionId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        tool_call_id: golutra_core::ToolCallId::new(),
        questions: vec![golutra_core::UserQuestionPrompt {
            id: "format".to_owned(),
            header: "Output".to_owned(),
            question: "Choose a format".to_owned(),
            mode: golutra_core::UserQuestionMode::Single,
            options: vec![
                golutra_core::UserQuestionOption {
                    id: "json".to_owned(),
                    label: "JSON".to_owned(),
                    description: None,
                },
                golutra_core::UserQuestionOption {
                    id: "text".to_owned(),
                    label: "Text".to_owned(),
                    description: None,
                },
            ],
        }],
    });
    dialog.focus_free_text(0);
    dialog.current_free_text_mut().insert_str("你");
    app.question_dialog = Some(dialog);
    let mut terminal = Terminal::new(TestBackend::new(40, 14)).expect("terminal");

    terminal
        .draw(|frame| {
            draw_question_dialog(
                frame,
                frame.area(),
                app.question_dialog.as_ref().expect("dialog"),
                &app,
            );
        })
        .expect("draw question");

    terminal
        .backend_mut()
        .assert_cursor_position(Position::new(6, 9));
    let rows = (0..14)
        .map(|row| {
            (0..40)
                .filter_map(|column| terminal.backend().buffer().cell((column, row)))
                .map(|cell| cell.symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    assert!(
        rows.iter().any(|row| row.starts_with("  ( ) JSON")),
        "option must not retain focus while free text is focused: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|row| row.starts_with("› Other answer / notes")),
        "free-text editor must display the focus marker: {rows:?}"
    );
}

#[test]
fn settings_model_editor_renders_the_real_unicode_cursor() {
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    let mut dialog = SettingsDialogState::new(
        &app.runtime_controls,
        &app.provider_choices,
        &app.preferences,
        false,
    );
    dialog.selected_row = SettingsRow::Model;
    dialog.editing_model = true;
    dialog.model_input.set_text("你ab");
    dialog.model_input.move_left();
    app.settings_dialog = Some(dialog);
    let mut terminal = Terminal::new(TestBackend::new(50, 20)).expect("terminal");

    terminal
        .draw(|frame| {
            draw_settings_dialog(
                frame,
                frame.area(),
                app.settings_dialog.as_ref().expect("settings"),
                &app,
            );
        })
        .expect("draw settings");

    terminal
        .backend_mut()
        .assert_cursor_position(Position::new(12, 5));
    assert!(!terminal_buffer_text(&terminal).contains("你ab|"));
}

#[test]
fn session_picker_filter_and_rename_render_real_unicode_cursors() {
    let app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    let mut picker = ResumePickerState::new(vec![resume_item("session")]);
    picker.search.set_text("你ab");
    picker.search.move_left();
    let mut filter = Terminal::new(TestBackend::new(50, 8)).expect("filter terminal");

    filter
        .draw(|frame| draw_resume_picker(frame, frame.area(), &picker, app.thread_id, &app))
        .expect("draw filter");
    filter
        .backend_mut()
        .assert_cursor_position(Position::new(28, 0));

    picker.begin_action(SessionPickerAction::Rename);
    picker.action_input.set_text("你ab");
    picker.action_input.move_left();
    let mut rename = Terminal::new(TestBackend::new(50, 8)).expect("rename terminal");
    rename
        .draw(|frame| draw_resume_picker(frame, frame.area(), &picker, app.thread_id, &app))
        .expect("draw rename");
    rename
        .backend_mut()
        .assert_cursor_position(Position::new(10, 1));
    assert!(!terminal_buffer_text(&rename).contains("你ab|"));
}

#[tokio::test]
async fn export_destination_supports_in_place_unicode_editing_and_a_real_cursor() {
    let transport = RuntimeTransport::in_memory().await.expect("transport");
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.export_flow = Some(ExportFlowState {
        picker: ResumePickerState::new(Vec::new()),
        step: ExportFlowStep::Destination,
        range_input: ComposerInput::from_text("1"),
        destination_input: ComposerInput::from_text("你b"),
        error: None,
        receipt: None,
    });

    for key in [KeyCode::Left, KeyCode::Char('a')] {
        handle_key(KeyEvent::new(key, KeyModifiers::NONE), &mut app, &transport)
            .await
            .expect("edit export destination");
    }
    assert_eq!(
        app.export_flow
            .as_ref()
            .expect("export")
            .destination_input
            .text(),
        "你ab"
    );

    let mut terminal = Terminal::new(TestBackend::new(50, 12)).expect("terminal");
    terminal
        .draw(|frame| {
            draw_export_flow(
                frame,
                frame.area(),
                app.export_flow.as_ref().expect("export"),
                app.thread_id,
                &app,
            );
        })
        .expect("draw export");

    assert_eq!(
        terminal
            .backend()
            .buffer()
            .cell((6, 3))
            .expect("CJK cell")
            .symbol(),
        "你"
    );
    assert_eq!(
        terminal
            .backend()
            .buffer()
            .cell((8, 3))
            .expect("a cell")
            .symbol(),
        "a"
    );
    assert_eq!(
        terminal
            .backend()
            .buffer()
            .cell((9, 3))
            .expect("b cell")
            .symbol(),
        "b"
    );
    terminal
        .backend_mut()
        .assert_cursor_position(Position::new(9, 3));
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
async fn debug_switch_candidate_executes_through_the_composer_enter_path() {
    let transport = RuntimeTransport::in_memory().await.expect("transport");
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.input.set_text("/debug switch");

    let candidates = app.slash_candidates();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].command, "/debug switch");

    handle_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .expect("execute debug switch");

    assert!(!app.developer_observations_expanded);
    assert!(app.input.is_empty());
    assert!(
        app.command_messages
            .iter()
            .all(|message| message.title != "Command error")
    );
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
    let candidates = app.slash_candidates();
    app.slash_selected = candidates
        .iter()
        .position(|candidate| candidate.command == "/resume")
        .expect("resume candidate");
    let lines = slash_candidate_lines(&app, &candidates)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(lines.contains("/new"));
    assert!(lines.contains("/resume"));
    assert!(lines.contains("› /resume"));
    assert_eq!(bottom_pane_height(&app), 8);
}

#[test]
fn debug_candidates_render_parent_and_switch_from_a_short_prefix() {
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.input.set_text("/d");

    let candidates = app.slash_candidates();
    let lines = slash_candidate_lines(&app, &candidates)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>();

    assert_eq!(candidates.len(), 4);
    assert!(lines[0].contains("/debug  toggle debug view"));
    assert!(lines[1].contains("/debug switch  toggle expanded or compact observations"));
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
        "[unrestricted] gpt-5.6-sol · /workspace"
    );
}

#[test]
fn session_settings_are_embedded_in_the_next_prompt_without_global_mutation() {
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (configured-model)".to_owned(),
        None,
    );
    app.set_session_model("session-model".to_owned());
    app.set_session_effort(ReasoningEffortSelection::Effort(
        ProviderReasoningEffort::High,
    ));
    app.set_permission_mode(true);

    let payload = app.runtime_prompt_payload("inspect".to_owned());
    assert_eq!(payload["provider_model"], "session-model");
    assert_eq!(
        payload["provider_generation_config"]["reasoning_effort"],
        "high"
    );
    assert_eq!(payload["yolo"], true);
    assert_eq!(payload["allow_network"], true);
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
fn debug_footer_does_not_render_a_facts_control() {
    for width in [40_u16, 80, 120] {
        let app = TuiApp::new(
            ThreadId::new(),
            SessionId::new(),
            None,
            true,
            "ready (mock)".to_owned(),
            None,
        )
        .with_footer_context(
            "/workspace/with/a/long/path/that/must/shrink/first",
            "gpt-5.6-sol-max-with-a-long-model-name",
        );
        let mut terminal = Terminal::new(TestBackend::new(width, 3)).expect("terminal");

        terminal
            .draw(|frame| draw_bottom_pane(frame, frame.area(), &app))
            .expect("draw debug footer");

        let footer = (0..width)
            .filter_map(|x| terminal.backend().buffer().cell((x, 2)))
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert_eq!(display_width(&footer), usize::from(width));
        assert!(!footer.contains("facts"), "{width}: {footer:?}");
    }
}

#[test]
fn footer_click_does_not_change_the_debug_view() {
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        true,
        "ready (mock)".to_owned(),
        None,
    );
    let mut terminal = Terminal::new(TestBackend::new(80, 12)).expect("terminal");
    terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("draw debug footer");
    let generation = app.history_replay_generation;
    let body_mode = app.layout.body_mode;

    handle_mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: app.layout.bottom.right().saturating_sub(3),
            row: app.layout.bottom.bottom().saturating_sub(1),
            modifiers: KeyModifiers::NONE,
        },
        &mut app,
    );

    assert_eq!(app.history_replay_generation, generation);
    assert_eq!(app.layout.body_mode, body_mode);
}

#[test]
fn runtime_modal_temporarily_replaces_search_surface_without_losing_query() {
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.open_transcript_search();
    app.transcript_search
        .as_mut()
        .expect("transcript search")
        .input
        .insert_str("needle");
    app.approval_dialog = Some(ApprovalDialogState::new(ApprovalRequest {
        approval_id: golutra_core::ApprovalId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        tool_call_id: golutra_core::ToolCallId::new(),
        tool_name: "shell".to_owned(),
        resource: "cargo test".to_owned(),
        reason: "process execution requires approval".to_owned(),
    }));

    let mut modal = Terminal::new(TestBackend::new(80, 5)).expect("modal terminal");
    modal
        .draw(|frame| draw_bottom_pane(frame, frame.area(), &app))
        .expect("draw modal surface");
    let modal_text = terminal_buffer_text(&modal);
    assert!(modal_text.contains("Resolve the pending tool request"));
    assert!(!modal_text.contains("Find: needle"));

    app.approval_dialog = None;
    let mut restored = Terminal::new(TestBackend::new(80, 4)).expect("search terminal");
    restored
        .draw(|frame| draw_bottom_pane(frame, frame.area(), &app))
        .expect("draw restored search");
    assert!(terminal_buffer_text(&restored).contains("Find: needle"));
}

#[test]
fn search_temporarily_hides_composer_accessories_and_restores_them() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("context.txt"), "context").expect("attachment");
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.workspace_path = workspace.path().to_path_buf();
    app.input.set_text("/");
    app.add_attachment("context.txt");
    app.open_history_search();

    let mut search = Terminal::new(TestBackend::new(80, 10)).expect("search terminal");
    search
        .draw(|frame| draw_bottom_pane(frame, frame.area(), &app))
        .expect("draw search");
    let search_text = terminal_buffer_text(&search);
    assert!(search_text.contains("History:"));
    assert!(!search_text.contains("/resume"));
    assert!(!search_text.contains("context.txt"));

    app.history_search = None;
    let mut composer = Terminal::new(TestBackend::new(80, 10)).expect("composer terminal");
    composer
        .draw(|frame| draw_bottom_pane(frame, frame.area(), &app))
        .expect("draw composer");
    let composer_text = terminal_buffer_text(&composer);
    assert!(composer_text.contains("/resume"));
    assert!(composer_text.contains("context.txt"));
}

#[tokio::test]
async fn runtime_prompt_stacks_above_user_workflows_and_restores_them() {
    let transport = RuntimeTransport::in_memory().await.expect("transport");
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.queue_picker = Some(QueuePickerState::default());
    app.approval_dialog = Some(ApprovalDialogState::new(ApprovalRequest {
        approval_id: golutra_core::ApprovalId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        tool_call_id: golutra_core::ToolCallId::new(),
        tool_name: "shell".to_owned(),
        resource: "cargo test".to_owned(),
        reason: "process execution requires approval".to_owned(),
    }));

    assert_eq!(app.overlay_surface(), Some(OverlaySurface::Approval));
    let regions = overlay_mouse_regions(Rect::new(0, 0, 80, 20), &app);
    assert!(
        regions
            .iter()
            .any(|region| matches!(region.press, UiMousePress::Approval(_)))
    );
    assert!(
        regions
            .iter()
            .all(|region| !matches!(region.press, UiMousePress::Queue(_)))
    );
    handle_key(
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .expect("route escape to approval");
    assert!(app.queue_picker.is_some());
    assert_eq!(
        app.approval_dialog
            .as_ref()
            .expect("approval")
            .selected_choice(),
        ApprovalChoice::Deny
    );

    app.approval_dialog = None;
    assert_eq!(app.overlay_surface(), Some(OverlaySurface::Queue));

    app.queue_picker = None;
    app.export_flow = Some(ExportFlowState {
        picker: ResumePickerState::new(Vec::new()),
        step: ExportFlowStep::SelectSession,
        range_input: ComposerInput::from_text("1"),
        destination_input: ComposerInput::default(),
        error: None,
        receipt: None,
    });
    app.question_dialog = Some(QuestionDialogState::new(UserQuestionRequest {
        question_id: golutra_core::QuestionId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        tool_call_id: golutra_core::ToolCallId::new(),
        questions: vec![golutra_core::UserQuestionPrompt {
            id: "format".to_owned(),
            header: "Output".to_owned(),
            question: "Choose an output format".to_owned(),
            mode: golutra_core::UserQuestionMode::Single,
            options: vec![
                golutra_core::UserQuestionOption {
                    id: "json".to_owned(),
                    label: "JSON".to_owned(),
                    description: None,
                },
                golutra_core::UserQuestionOption {
                    id: "text".to_owned(),
                    label: "Text".to_owned(),
                    description: None,
                },
            ],
        }],
    }));

    assert_eq!(app.overlay_surface(), Some(OverlaySurface::Question));
    handle_key(
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .expect("route escape to question");
    assert!(app.export_flow.is_some());
    assert!(app.question_dialog.is_some());

    app.question_dialog = None;
    assert_eq!(app.overlay_surface(), Some(OverlaySurface::Export));

    let task_id = TaskId::new();
    app.apply_runtime_refresh_snapshot(RuntimeRefreshSnapshot {
        binding: app.runtime_refresh_binding(),
        projection: UserProjection {
            session_id: app.session_id,
            task_id: Some(task_id),
            status: golutra_core::TaskStatus::WaitingAuthentication,
            visible_steps: Vec::new(),
            pending_approval: None,
            final_message: None,
            residual_risks: Vec::new(),
        },
        provider_status: None,
        developer_projection: None,
        remote: false,
    });
    assert!(app.auth_dialog.is_some());
    assert!(app.export_flow.is_some());
    assert_eq!(app.overlay_surface(), Some(OverlaySurface::Auth));

    app.open_help(HelpTopic::Overview);
    assert_eq!(app.overlay_surface(), Some(OverlaySurface::Help));
    assert_eq!(status_chip(&app), "help");
}

#[test]
fn contextual_help_reports_the_surface_beneath_it() {
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        Some(AuthDialogState::new()),
    );

    app.open_help(HelpTopic::Overview);
    assert_eq!(
        app.help_dialog.as_ref().expect("auth help").context,
        "provider setup"
    );

    app.help_dialog = None;
    app.auth_dialog = None;
    app.open_transcript_search();
    app.open_help(HelpTopic::Overview);
    assert_eq!(
        app.help_dialog.as_ref().expect("search help").context,
        "transcript search"
    );
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
    let app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    assert_eq!(role_marker(&app, &TranscriptRole::User), "› ");
    assert_eq!(role_marker(&app, &TranscriptRole::Assistant), "• ");
    assert_eq!(role_marker(&app, &TranscriptRole::Status), "• ");
    assert_eq!(role_marker(&app, &TranscriptRole::System), "• ");
}

#[test]
fn user_and_assistant_messages_start_on_the_marker_line() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        Some(task_id),
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
            json!({"payload": {"prompt": "第一行\n第二行"}}),
        ),
        transcript_event(
            2,
            session_id,
            task_id,
            RuntimeEventType::AssistantMessage,
            json!({"content": "直接回答"}),
        ),
    ];

    let lines = full_transcript_layout(&app, Rect::new(0, 0, 80, 12)).plain_lines();

    assert!(lines.iter().any(|line| line == "› 第一行"));
    assert!(lines.iter().any(|line| line == "  第二行"));
    assert!(lines.iter().any(|line| line == "• 直接回答"));
    assert!(!lines.iter().any(|line| line.contains("You")));
    assert!(!lines.iter().any(|line| line.contains("Golutra")));
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

#[tokio::test]
async fn question_mark_is_typed_into_modal_inputs_instead_of_opening_help() {
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

    handle_key(
        KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .expect("type question mark");

    assert_eq!(app.auth_dialog.as_ref().expect("dialog").api_key, "?");
    assert!(app.help_dialog.is_none());
}

#[test]
fn bracketed_paste_routes_to_the_active_text_surface() {
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
    app.composer_mode = ComposerMode::VimNormal;
    handle_paste("sk-?key\n", &mut app);
    assert_eq!(app.auth_dialog.as_ref().expect("dialog").api_key, "sk-?key");

    app.auth_dialog = None;
    app.prompt_history.record("find this prompt");
    app.open_history_search();
    handle_paste("find\nthis", &mut app);
    let search = app.history_search.as_ref().expect("history search");
    assert_eq!(search.input.text(), "find this");
    assert_eq!(search.matches, vec!["find this prompt"]);

    app.history_search = None;
    app.resume_picker = Some(ResumePickerState::new(vec![resume_item("search target")]));
    handle_paste("target", &mut app);
    let picker = app.resume_picker.as_ref().expect("resume picker");
    assert_eq!(picker.search.text(), "target");
    assert_eq!(picker.items.len(), 1);
}

#[test]
fn help_dialog_captures_paste_before_hidden_input_surfaces() {
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.resume_picker = Some(ResumePickerState::new(vec![resume_item("search target")]));
    app.help_dialog = Some(HelpDialogState::new(HelpTopic::Overview, "resume picker"));

    handle_paste("hidden resume search", &mut app);

    assert!(
        app.resume_picker
            .as_ref()
            .expect("resume picker")
            .search
            .is_empty()
    );

    app.resume_picker = None;
    app.question_dialog = Some(QuestionDialogState::new(UserQuestionRequest {
        question_id: golutra_core::QuestionId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        tool_call_id: golutra_core::ToolCallId::new(),
        questions: vec![golutra_core::UserQuestionPrompt {
            id: "format".to_owned(),
            header: "Output".to_owned(),
            question: "Choose an output format".to_owned(),
            mode: golutra_core::UserQuestionMode::Single,
            options: vec![
                golutra_core::UserQuestionOption {
                    id: "json".to_owned(),
                    label: "JSON".to_owned(),
                    description: None,
                },
                golutra_core::UserQuestionOption {
                    id: "text".to_owned(),
                    label: "Text".to_owned(),
                    description: None,
                },
            ],
        }],
    }));

    handle_paste("hidden question answer", &mut app);

    let question = app.question_dialog.as_ref().expect("question dialog");
    assert!(!question.is_free_text_focused());
    assert!(question.current_free_text().is_empty());
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
    let rendered = terminal_buffer_text(&terminal);
    assert!(!rendered.contains("Transcript •"));
    assert!(!rendered.contains("F1 help"));
    assert_eq!(app.layout.body.y, 0);
}

#[test]
fn debug_layout_keeps_transcript_left_and_observations_right_at_every_width() {
    let app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        true,
        "ready (mock)".to_owned(),
        None,
    );

    let wide = ui_layout(Rect::new(0, 0, 120, 30), &app);
    assert_eq!(wide.body_mode, BodyLayoutMode::ResponseAndDeveloper);
    let wide_developer = wide.developer.expect("wide observation pane");
    assert_eq!(wide.transcript.x, wide.body.x);
    assert_eq!(wide.transcript.width, wide.body.width / 2);
    assert_eq!(wide_developer.x, wide.transcript.right());
    assert_eq!(
        wide_developer.width,
        wide.body.width - wide.transcript.width
    );
    assert_eq!(wide_developer.right(), wide.body.right());
    assert_eq!(wide.transcript.height, wide.body.height);
    assert_eq!(wide_developer.height, wide.body.height);

    let narrow = ui_layout(Rect::new(0, 0, 80, 30), &app);
    assert_eq!(narrow.body_mode, BodyLayoutMode::ResponseAndDeveloper);
    let narrow_developer = narrow.developer.expect("narrow observation pane");
    assert_eq!(narrow.transcript.x, narrow.body.x);
    assert_eq!(narrow.transcript.width, narrow.body.width / 2);
    assert_eq!(narrow_developer.x, narrow.transcript.right());
    assert_eq!(
        narrow_developer.width,
        narrow.body.width - narrow.transcript.width
    );
    assert_eq!(narrow_developer.right(), narrow.body.right());
    assert_eq!(narrow.transcript.height, narrow.body.height);
    assert_eq!(narrow_developer.height, narrow.body.height);
}

#[test]
fn debug_live_timeline_never_renders_transcript_and_observation_on_the_same_row() {
    for width in [80, 81, 120, 121] {
        let session_id = SessionId::new();
        let task_id = TaskId::new();
        let turn_id = TurnId::new();
        let mut created = transcript_event(
            1,
            session_id,
            task_id,
            RuntimeEventType::TaskCreated,
            json!({"payload": {"prompt": "恢复后的长中文正文".repeat(24)}}),
        );
        created.turn_id = Some(turn_id);
        let mut streamed = transcript_event(
            2,
            session_id,
            task_id,
            RuntimeEventType::ProviderStreamed,
            json!({"delta": {"kind": "text_delta", "text": "模型流式输出内容".repeat(36)}}),
        );
        streamed.turn_id = Some(turn_id);
        let events = vec![created, streamed];
        let mut app = TuiApp::new(
            ThreadId::new(),
            session_id,
            Some(task_id),
            true,
            "ready (mock)".to_owned(),
            None,
        );
        app.events = events.clone();
        app.developer_projection = Some(debug_projection_with_events(
            session_id,
            Some(task_id),
            events,
        ));
        app.enable_inline_history();
        let mut terminal = Terminal::new(TestBackend::new(width, 44)).expect("terminal");

        terminal
            .draw(|frame| draw_ui(frame, &mut app))
            .expect("draw debug timeline");

        let buffer = terminal.backend().buffer();
        let transcript = app.layout.transcript;
        let developer = app.layout.developer.expect("developer half");
        let mut transcript_rows = 0_usize;
        let mut observation_rows = 0_usize;
        for row in app.layout.body.top()..app.layout.body.bottom() {
            let left_has_content = (transcript.left()..transcript.right()).any(|column| {
                buffer
                    .cell((column, row))
                    .is_some_and(|cell| !cell.symbol().trim().is_empty())
            });
            let right_has_content = (developer.left()..developer.right()).any(|column| {
                buffer
                    .cell((column, row))
                    .is_some_and(|cell| !cell.symbol().trim().is_empty())
            });
            assert!(
                !(left_has_content && right_has_content),
                "width {width} rendered both panes on terminal row {row}"
            );
            transcript_rows += usize::from(left_has_content);
            observation_rows += usize::from(right_has_content);
        }
        assert!(transcript_rows > 0, "width {width} lost transcript rows");
        assert!(observation_rows > 0, "width {width} lost observation rows");
    }
}

#[test]
fn debug_transcript_switch_requests_a_source_backed_rebuild() {
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        true,
        "ready (mock)".to_owned(),
        None,
    );
    let mut terminal = Terminal::new(TestBackend::new(120, 24)).expect("terminal");
    terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("draw debug events");
    assert_eq!(app.layout.body_mode, BodyLayoutMode::ResponseAndDeveloper);
    let initial_generation = app.history_replay_generation;

    app.toggle_transcript_fullscreen();
    terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("draw full transcript");
    assert_eq!(app.layout.body_mode, BodyLayoutMode::Transcript);
    assert_eq!(app.history_replay_generation, initial_generation + 1);

    app.toggle_transcript_fullscreen();
    assert_eq!(app.body_view_mode, BodyViewMode::Auto);
    assert_eq!(app.history_replay_generation, initial_generation + 2);
}

#[test]
fn transcript_search_finds_logical_rows_and_moves_the_viewport() {
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.command_messages = (0..30)
        .map(|index| TranscriptItem {
            role: TranscriptRole::System,
            title: format!("message {index}"),
            body: vec![if index == 3 || index == 27 {
                format!("needle {index}")
            } else {
                "body".to_owned()
            }],
        })
        .collect();
    let mut terminal = Terminal::new(TestBackend::new(80, 16)).expect("terminal");
    terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("draw transcript");

    app.open_transcript_search();
    app.transcript_search
        .as_mut()
        .expect("search")
        .input
        .set_text("needle");
    app.rebuild_transcript_search();
    let search = app.transcript_search.as_ref().expect("search");
    assert_eq!(search.matches.len(), 2);
    assert_eq!(search.current_line(), Some(10));
    assert!(app.transcript_scroll.offset_from_bottom > 0);
    let first_offset = app.transcript_scroll.offset_from_bottom;

    app.transcript_search
        .as_mut()
        .expect("search")
        .select_next();
    app.focus_current_search_match();
    assert!(app.transcript_scroll.offset_from_bottom < first_offset);
    let layout = transcript_layout(&app, app.layout.body);
    let target = layout
        .visual_start_for_line(
            app.transcript_search
                .as_ref()
                .and_then(TranscriptSearchState::current_line)
                .expect("selected result"),
        )
        .expect("result row");
    assert!(
        layout
            .visible_window(
                app.layout.body.height.saturating_sub(1) as usize,
                app.transcript_scroll.offset_from_bottom,
                app.transcript_top_row_override,
            )
            .contains(&target)
    );
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

    app.set_debug_mode(true);
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
    assert!(!developer_text.contains("Developer runtime"));
    assert!(!developer_text.contains("▸ facts"));
    assert!(!developer_text.contains("▾ facts"));
    assert!(developer_text.contains("verify Pass"));
    assert!(developer_text.contains("diagnosis Tool/tool_failed"));

    let layout = app.layout;
    let developer_area = layout.developer.expect("developer area");
    assert_eq!(layout.body_mode, BodyLayoutMode::ResponseAndDeveloper);
    assert_eq!(developer_area.x, layout.transcript.right());
    assert_eq!(layout.transcript.width, layout.body.width / 2);
    assert_eq!(
        developer_area.width,
        layout.body.width - layout.transcript.width
    );
    assert!(layout.transcript.width > 0);
}

#[test]
fn developer_event_details_stay_compact_in_the_observation_pane() {
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
    app.developer_observations_expanded = false;
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");

    terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("draw compact developer event");
    let rendered = terminal_buffer_text(&terminal);
    assert!(!rendered.contains("▸ facts"));
    assert!(!rendered.contains("▾ facts"));
    assert!(rendered.contains('…'));
    assert!(!rendered.contains(tail));
}

#[test]
fn developer_live_surface_follows_the_latest_event() {
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
                    "summary": format!("event-{sequence_no} {}", "detail ".repeat(20))
                }),
            )
        })
        .collect::<Vec<_>>();
    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        Some(task_id),
        true,
        "ready (mock)".to_owned(),
        None,
    );
    app.events = events.clone();
    app.developer_projection = Some(debug_projection_with_events(
        session_id,
        Some(task_id),
        events,
    ));
    app.enable_inline_history();
    let mut terminal = Terminal::new(TestBackend::new(80, 10)).expect("terminal");
    terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("draw latest developer events");
    let rendered = terminal_buffer_text(&terminal);
    assert!(rendered.contains("#30 StepCompleted/Runtime"), "{rendered}");
    assert!(!rendered.contains("#1 StepCompleted/Runtime"), "{rendered}");
    assert!(rendered.contains("event-30"), "{rendered}");
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
async fn debug_commands_keep_mode_and_observation_detail_independent() {
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
    assert!(app.developer_observations_expanded);

    app.events.push(transcript_event(
        1,
        app.session_id,
        TaskId::new(),
        RuntimeEventType::AssistantMessage,
        json!({"content": "ordinary history must not reload"}),
    ));
    let ordinary_generation = app.history_replay_generation;
    app.execute_slash_command(
        &transport,
        SlashCommand::Debug(SlashDebugCommand::ToggleObservationDetail),
    )
    .await
    .expect("change future debug detail");
    assert!(!app.debug_mode);
    assert!(!app.developer_observations_expanded);
    assert!(app.developer_projection.is_none());
    assert_eq!(app.events.len(), 1);
    assert_eq!(app.history_replay_generation, ordinary_generation);

    app.execute_slash_command(
        &transport,
        SlashCommand::Debug(SlashDebugCommand::ToggleView),
    )
    .await
    .expect("enable debug");
    assert!(app.debug_mode);
    assert!(!app.developer_observations_expanded);
    assert!(app.developer_projection.is_some());
    assert!(app.developer_error.is_none());
    assert!(app.events.is_empty());

    app.events.push(transcript_event(
        1,
        app.session_id,
        TaskId::new(),
        RuntimeEventType::AssistantMessage,
        json!({"content": "local history must be replaced"}),
    ));
    let generation = app.history_replay_generation;
    app.execute_slash_command(
        &transport,
        SlashCommand::Debug(SlashDebugCommand::ToggleObservationDetail),
    )
    .await
    .expect("expand and reload debug");
    assert!(app.debug_mode);
    assert!(app.developer_observations_expanded);
    assert!(app.events.is_empty());
    assert_eq!(app.history_replay_generation, generation + 1);

    app.events.push(transcript_event(
        2,
        app.session_id,
        TaskId::new(),
        RuntimeEventType::AssistantMessage,
        json!({"content": "debug history must be replaced when leaving"}),
    ));
    let generation = app.history_replay_generation;
    app.execute_slash_command(
        &transport,
        SlashCommand::Debug(SlashDebugCommand::ToggleView),
    )
    .await
    .expect("disable debug");
    assert!(!app.debug_mode);
    assert!(app.developer_observations_expanded);
    assert!(app.developer_projection.is_none());
    assert!(app.developer_error.is_none());
    assert!(app.events.is_empty());
    assert_eq!(app.history_replay_generation, generation + 1);

    let generation = app.history_replay_generation;
    app.execute_slash_command(
        &transport,
        SlashCommand::Debug(SlashDebugCommand::ToggleObservationDetail),
    )
    .await
    .expect("change future debug detail again");
    assert!(!app.debug_mode);
    assert!(!app.developer_observations_expanded);
    assert!(app.developer_projection.is_none());
    assert_eq!(app.history_replay_generation, generation);
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
    app.set_debug_mode(true);
    app.refresh(&transport).await.expect("debug refresh");
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

    app.set_debug_mode(true);
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

fn terminal_buffer_rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
    let area = terminal.backend().buffer().area;
    (area.top()..area.bottom())
        .map(|row| {
            (area.left()..area.right())
                .filter_map(|column| terminal.backend().buffer().cell((column, row)))
                .map(|cell| cell.symbol())
                .collect::<String>()
        })
        .collect()
}

fn terminal_buffer_display_rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
    let buffer = terminal.backend().buffer();
    let area = buffer.area;
    (area.top()..area.bottom())
        .map(|row| {
            let mut rendered = String::new();
            let mut column = area.left();
            while column < area.right() {
                let Some(cell) = buffer.cell((column, row)) else {
                    break;
                };
                rendered.push_str(cell.symbol());
                column = column.saturating_add(
                    u16::try_from(display_width(cell.symbol()).max(1)).unwrap_or(u16::MAX),
                );
            }
            rendered
        })
        .collect()
}

fn draw_inline_test_frame(terminal: &mut Terminal<TestBackend>, app: &mut TuiApp) {
    terminal
        .draw(|frame| draw_ui(frame, app))
        .expect("draw inline frame");
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

    app.enable_inline_history();
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
fn transcript_detail_reflow_keeps_the_first_visible_projection() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        Some(task_id),
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.events = (1..=24)
        .map(|sequence_no| {
            transcript_event(
                sequence_no,
                session_id,
                task_id,
                RuntimeEventType::ToolCompleted,
                json!({
                    "envelope": {
                        "tool_call_id": golutra_core::ToolCallId::new(),
                        "tool_name": "edit_file",
                        "status": "ok",
                        "summary": format!("file {sequence_no} edited"),
                        "structured_facts": {}
                    },
                    "diff_previews": [{
                        "path": format!("src/file_{sequence_no}.rs"),
                        "lines": [
                            format!("-old line {sequence_no}"),
                            format!("+new line {sequence_no}"),
                            "additional detail that remains associated with this operation"
                        ],
                        "truncated": false
                    }]
                }),
            )
        })
        .collect();
    let mut terminal = Terminal::new(TestBackend::new(72, 18)).expect("terminal");
    terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("draw collapsed transcript");
    let visible_rows = app.layout.transcript.height.saturating_sub(1) as usize;
    app.scroll_transcript(TranscriptScrollAction::PageUp, visible_rows);
    let anchor = app
        .first_visible_transcript_projection()
        .expect("collapsed anchor");

    app.toggle_transcript_details();
    assert_eq!(app.first_visible_transcript_projection(), Some(anchor));

    app.toggle_transcript_details();
    assert_eq!(app.first_visible_transcript_projection(), Some(anchor));

    app.toggle_transcript_details();
    let expanded = transcript_layout(&app, app.layout.transcript);
    let tail_projection = 22;
    let tail_row = expanded
        .visual_start_for_projection(tail_projection)
        .expect("tail projection row");
    app.transcript_scroll.row_count = expanded.row_count;
    app.set_transcript_top_row(&expanded, tail_row, visible_rows);
    assert_eq!(
        app.first_visible_transcript_projection(),
        Some(tail_projection)
    );

    app.toggle_transcript_details();
    assert_eq!(
        app.first_visible_transcript_projection(),
        Some(tail_projection)
    );
}

#[test]
fn transcript_width_reflow_keeps_the_first_visible_projection() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        Some(task_id),
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.events = (1..=30)
        .map(|sequence_no| {
            transcript_event(
                sequence_no,
                session_id,
                task_id,
                RuntimeEventType::StepCompleted,
                json!({
                    "summary": format!(
                        "event {sequence_no} {}",
                        "has enough content to wrap when the transcript narrows ".repeat(3)
                    )
                }),
            )
        })
        .collect();
    let mut wide = Terminal::new(TestBackend::new(100, 20)).expect("wide terminal");
    wide.draw(|frame| draw_ui(frame, &mut app))
        .expect("draw wide transcript");
    let visible_rows = app.layout.transcript.height.saturating_sub(1) as usize;
    app.scroll_transcript(TranscriptScrollAction::PageUp, visible_rows);
    let anchor = app
        .first_visible_transcript_projection()
        .expect("wide anchor");

    let mut narrow = Terminal::new(TestBackend::new(54, 20)).expect("narrow terminal");
    narrow
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("draw narrow transcript");

    assert_eq!(app.first_visible_transcript_projection(), Some(anchor));
}

#[test]
fn transcript_height_reflow_keeps_the_first_visible_logical_row() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        Some(task_id),
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.events = (1..=36)
        .map(|sequence_no| {
            transcript_event(
                sequence_no,
                session_id,
                task_id,
                RuntimeEventType::StepCompleted,
                json!({"summary": format!("stable logical row {sequence_no}")}),
            )
        })
        .collect();
    let mut tall = Terminal::new(TestBackend::new(80, 28)).expect("tall terminal");
    tall.draw(|frame| draw_ui(frame, &mut app))
        .expect("draw tall transcript");
    let visible_rows = app.layout.transcript.height.saturating_sub(1) as usize;
    app.scroll_transcript(TranscriptScrollAction::PageUp, visible_rows);
    let anchor = app
        .first_visible_transcript_anchor()
        .expect("tall transcript anchor");

    let mut short = Terminal::new(TestBackend::new(80, 14)).expect("short terminal");
    short
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("draw short transcript");

    let resized = app
        .first_visible_transcript_anchor()
        .expect("short transcript anchor");
    assert_eq!(resized.projection, anchor.projection);
    assert_eq!(resized.visual_offset, anchor.visual_offset);
}

#[test]
fn cancelling_an_earlier_turn_keeps_the_visible_projection_anchored() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let cancelled_turn = TurnId::new();
    let mut cancelled = transcript_event(
        1,
        session_id,
        task_id,
        RuntimeEventType::TurnQueued,
        json!({"payload": {"prompt": "cancel this queued turn"}}),
    );
    cancelled.turn_id = Some(cancelled_turn);
    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        Some(task_id),
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.events = std::iter::once(cancelled)
        .chain((2..=18).map(|sequence_no| {
            transcript_event(
                sequence_no,
                session_id,
                task_id,
                RuntimeEventType::ToolCompleted,
                json!({
                    "envelope": {
                        "tool_call_id": golutra_core::ToolCallId::new(),
                        "tool_name": "read_file",
                        "status": "ok",
                        "summary": format!("unchanged operation {sequence_no}"),
                        "structured_facts": {}
                    }
                }),
            )
        }))
        .collect();
    let mut terminal = Terminal::new(TestBackend::new(72, 18)).expect("terminal");
    terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("draw transcript");
    let layout = transcript_layout(&app, app.layout.transcript);
    let top_row = layout
        .visual_start_for_projection(8)
        .expect("anchor projection row");
    let visible_rows = app.layout.transcript.height.saturating_sub(1) as usize;
    app.transcript_scroll.row_count = layout.row_count;
    app.set_transcript_top_row(&layout, top_row, visible_rows);
    let anchor = app
        .first_visible_transcript_anchor()
        .expect("pre-cancellation anchor");

    let mut cancellation = transcript_event(
        19,
        session_id,
        task_id,
        RuntimeEventType::TurnCancelled,
        json!({"summary": "queued turn cancelled"}),
    );
    cancellation.turn_id = Some(cancelled_turn);
    app.apply_runtime_event(cancellation);

    let after = app
        .first_visible_transcript_anchor()
        .expect("post-cancellation anchor");
    assert_eq!(after.projection, anchor.projection);
    assert_eq!(after.visual_offset, anchor.visual_offset);
    assert_eq!(after.original_index + 1, anchor.original_index);
}

#[test]
fn transcript_override_paging_stops_at_the_last_full_viewport() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        Some(task_id),
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.events = (1..=30)
        .map(|sequence_no| {
            transcript_event(
                sequence_no,
                session_id,
                task_id,
                RuntimeEventType::StepCompleted,
                json!({"summary": format!("transcript row {sequence_no}")}),
            )
        })
        .collect();
    let mut terminal = Terminal::new(TestBackend::new(120, 22)).expect("terminal");
    terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("draw");

    let transcript_layout = transcript_layout(&app, app.layout.transcript);
    let transcript_rows = app.layout.transcript.height.saturating_sub(1) as usize;
    app.transcript_scroll.row_count = transcript_layout.row_count;
    app.set_transcript_top_row(
        &transcript_layout,
        transcript_layout.row_count.saturating_sub(1),
        transcript_rows,
    );
    app.scroll_transcript(TranscriptScrollAction::PageDown, transcript_rows);
    let transcript_window = transcript_layout.visible_window(
        transcript_rows,
        app.transcript_scroll.offset_from_bottom,
        app.transcript_top_row_override,
    );
    assert_eq!(
        transcript_window.start,
        transcript_layout.row_count.saturating_sub(transcript_rows)
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
fn inline_history_commits_only_the_stable_projection_prefix() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let turn_id = TurnId::new();
    let mut created = transcript_event(
        1,
        session_id,
        task_id,
        RuntimeEventType::TaskCreated,
        json!({"payload": {"prompt": "inspect the workspace"}}),
    );
    created.turn_id = Some(turn_id);
    let mut streamed = transcript_event(
        2,
        session_id,
        task_id,
        RuntimeEventType::ProviderStreamed,
        json!({"delta": {"kind": "text_delta", "text": "Working"}}),
    );
    streamed.turn_id = Some(turn_id);
    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.events = vec![created, streamed];
    app.projection = Some(UserProjection {
        session_id,
        task_id: Some(task_id),
        status: golutra_core::TaskStatus::Running,
        visible_steps: Vec::new(),
        pending_approval: None,
        final_message: None,
        residual_risks: Vec::new(),
    });

    assert_eq!(stable_event_operation_projection_count(&app.events), 1);
    app.enable_inline_history();
    app.set_inline_history_committed_event_ids(HashSet::from([app.events[0].id]));
    let live = transcript_render_rows(&app)
        .into_iter()
        .map(|row| row.line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!live.contains("inspect the workspace"));
    assert!(live.contains("Working"));

    let full = full_transcript_layout(&app, Rect::new(0, 0, 80, 12)).plain_text();
    assert!(full.contains("inspect the workspace"));
    assert!(full.contains("Working"));

    let mut completed = transcript_event(
        3,
        session_id,
        task_id,
        RuntimeEventType::AssistantMessage,
        json!({"content": "Working complete."}),
    );
    completed.turn_id = Some(turn_id);
    app.events.push(completed);
    assert_eq!(stable_event_operation_projection_count(&app.events), 2);
}

#[test]
fn debug_scrollback_stops_at_an_unstable_transcript_operation() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let tool_call_id = golutra_core::ToolCallId::new();
    let mut events = (1..=24)
        .map(|sequence_no| {
            transcript_event(
                sequence_no,
                session_id,
                task_id,
                RuntimeEventType::AssistantMessage,
                json!({"content": format!("stable response {sequence_no}")}),
            )
        })
        .collect::<Vec<_>>();
    let tool_started = transcript_event(
        25,
        session_id,
        task_id,
        RuntimeEventType::ToolStarted,
        json!({
            "tool_call_id": tool_call_id,
            "tool_name": "shell",
            "arguments": {"command": "cargo test"}
        }),
    );
    let tool_started_id = tool_started.id;
    events.push(tool_started);
    events.extend((26..=40).map(|sequence_no| {
        transcript_event(
            sequence_no,
            session_id,
            task_id,
            RuntimeEventType::StepCompleted,
            json!({"summary": format!("later observation {sequence_no}")}),
        )
    }));
    let initial_events = events.clone();
    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        Some(task_id),
        true,
        "ready (mock)".to_owned(),
        None,
    );
    app.events = events;
    app.developer_observations_expanded = false;
    app.developer_projection = Some(debug_projection_with_events(
        session_id,
        Some(task_id),
        initial_events.clone(),
    ));
    app.enable_inline_history();
    let mut terminal = Terminal::with_options(
        TestBackend::new(100, 80),
        TerminalOptions {
            viewport: Viewport::Inline(12),
        },
    )
    .expect("inline terminal");
    let mut history = InlineHistoryState::new(session_id);

    history
        .flush(&mut terminal, &mut app)
        .expect("history before tool completion");
    assert!(!app.inline_history_committed_event_ids.is_empty());
    assert!(
        !app.inline_history_committed_event_ids
            .contains(&tool_started_id)
    );
    assert!(
        initial_events[24..]
            .iter()
            .all(|event| !app.inline_history_committed_event_ids.contains(&event.id)),
        "events after an unstable operation must remain in the live tail"
    );

    app.events.push(transcript_event(
        41,
        session_id,
        task_id,
        RuntimeEventType::ToolCompleted,
        json!({
            "envelope": {
                "tool_call_id": tool_call_id,
                "tool_name": "shell",
                "status": "ok",
                "summary": "tests passed",
                "structured_facts": {"command": "cargo test"}
            }
        }),
    ));
    app.events.extend((42..=55).map(|sequence_no| {
        transcript_event(
            sequence_no,
            session_id,
            task_id,
            RuntimeEventType::StepCompleted,
            json!({"summary": format!("settled observation {sequence_no}")}),
        )
    }));

    history
        .flush(&mut terminal, &mut app)
        .expect("history after tool completion");
    assert!(
        app.inline_history_committed_event_ids
            .contains(&tool_started_id),
        "the completed operation should become eligible for scrollback"
    );
}

#[test]
fn terminal_task_event_stabilizes_incomplete_activity_for_scrollback() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let tool_call_id = golutra_core::ToolCallId::new();
    let events = vec![
        transcript_event(
            1,
            session_id,
            task_id,
            RuntimeEventType::TaskCreated,
            json!({"payload": {"prompt": "run diagnostics"}}),
        ),
        transcript_event(
            2,
            session_id,
            task_id,
            RuntimeEventType::ToolStarted,
            json!({
                "tool_call_id": tool_call_id,
                "tool_name": "shell",
                "arguments": {"command": "cargo test"}
            }),
        ),
        transcript_event(
            3,
            session_id,
            task_id,
            RuntimeEventType::TaskInterrupted,
            json!({"summary": "task interrupted"}),
        ),
    ];

    assert_eq!(
        stable_event_operation_projection_count(&events),
        event_operation_projections(&events).len()
    );
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
fn debug_mouse_wheel_leaves_history_scrolling_to_the_terminal() {
    let session_id = SessionId::new();
    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        None,
        true,
        "ready (mock)".to_owned(),
        None,
    );
    app.developer_projection = Some(debug_projection_with_events(session_id, None, Vec::new()));
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");
    terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("draw");
    let developer_area = app.layout.developer.expect("developer area");
    let generation = app.history_replay_generation;

    handle_mouse(
        MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: developer_area.x + 1,
            row: developer_area.y + 1,
            modifiers: KeyModifiers::NONE,
        },
        &mut app,
    );
    assert_eq!(app.transcript_scroll.offset_from_bottom, 0);
    assert_eq!(app.history_replay_generation, generation);
}

#[test]
fn overlay_mouse_clicks_select_every_interactive_surface() {
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.layout.transcript = Rect::new(0, 1, 120, 24);

    app.approval_dialog = Some(ApprovalDialogState::new(ApprovalRequest {
        approval_id: golutra_core::ApprovalId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        tool_call_id: golutra_core::ToolCallId::new(),
        tool_name: "shell".to_owned(),
        resource: "cargo test".to_owned(),
        reason: "process execution requires approval".to_owned(),
    }));
    let deny = overlay_mouse_regions(app.layout.transcript, &app)
        .into_iter()
        .find(|region| region.press == UiMousePress::Approval(ApprovalChoice::Deny))
        .expect("deny region");
    assert_eq!(
        click_overlay(&mut app, deny.area.x, deny.area.y),
        Some(UiMouseActivation::Approval(ApprovalChoice::Deny))
    );
    assert_eq!(
        app.approval_dialog
            .as_ref()
            .expect("approval dialog")
            .selected_choice(),
        ApprovalChoice::Deny
    );
    app.approval_dialog = None;

    app.question_dialog = Some(QuestionDialogState::new(UserQuestionRequest {
        question_id: golutra_core::QuestionId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        tool_call_id: golutra_core::ToolCallId::new(),
        questions: vec![golutra_core::UserQuestionPrompt {
            id: "format".to_owned(),
            header: "Output".to_owned(),
            question: "Choose an output format".to_owned(),
            mode: golutra_core::UserQuestionMode::Single,
            options: vec![
                golutra_core::UserQuestionOption {
                    id: "json".to_owned(),
                    label: "JSON".to_owned(),
                    description: Some("structured output".to_owned()),
                },
                golutra_core::UserQuestionOption {
                    id: "text".to_owned(),
                    label: "Text".to_owned(),
                    description: None,
                },
            ],
        }],
    }));
    let option = overlay_mouse_regions(app.layout.transcript, &app)
        .into_iter()
        .find(|region| {
            region.press
                == UiMousePress::QuestionOption {
                    question: 0,
                    option: 1,
                }
        })
        .expect("question option region");
    assert_eq!(click_overlay(&mut app, option.area.x, option.area.y), None);
    assert!(
        app.question_dialog
            .as_ref()
            .expect("question dialog")
            .is_selected(1)
    );
    let free_text = overlay_mouse_regions(app.layout.transcript, &app)
        .into_iter()
        .find(|region| region.press == UiMousePress::QuestionFreeText { question: 0 })
        .expect("question free text region");
    assert_eq!(
        click_overlay(&mut app, free_text.area.x, free_text.area.y),
        None
    );
    assert!(
        app.question_dialog
            .as_ref()
            .expect("question dialog")
            .is_free_text_focused()
    );
    let submit = overlay_mouse_regions(app.layout.transcript, &app)
        .into_iter()
        .find(|region| region.press == UiMousePress::QuestionSubmit)
        .expect("question submit region");
    assert_eq!(
        click_overlay(&mut app, submit.area.x, submit.area.y),
        Some(UiMouseActivation::QuestionSubmit)
    );
    app.question_dialog = None;

    app.auth_dialog = Some(AuthDialogState::new());
    let custom = overlay_mouse_regions(app.layout.transcript, &app)
        .into_iter()
        .find(|region| region.press == UiMousePress::Auth(2))
        .expect("custom provider region");
    assert_eq!(
        click_overlay(&mut app, custom.area.x, custom.area.y),
        Some(UiMouseActivation::AuthContinue)
    );
    assert_eq!(app.auth_dialog.as_ref().expect("auth dialog").selected, 2);
    app.auth_dialog = None;

    app.resume_picker = Some(ResumePickerState::new(vec![
        resume_item("first"),
        resume_item("second"),
    ]));
    let second = overlay_mouse_regions(app.layout.transcript, &app)
        .into_iter()
        .find(|region| region.press == UiMousePress::Resume(1))
        .expect("second session region");
    assert_eq!(
        click_overlay(&mut app, second.area.x, second.area.y),
        Some(UiMouseActivation::ResumeSession)
    );
    assert_eq!(
        app.resume_picker.as_ref().expect("resume picker").selected,
        1
    );
    app.resume_picker = None;

    app.export_flow = Some(ExportFlowState {
        picker: ResumePickerState::new(vec![resume_item("first"), resume_item("second")]),
        step: ExportFlowStep::SelectSession,
        range_input: ComposerInput::from_text("1"),
        destination_input: ComposerInput::default(),
        error: None,
        receipt: None,
    });
    let second = overlay_mouse_regions(app.layout.transcript, &app)
        .into_iter()
        .find(|region| region.press == UiMousePress::Resume(1))
        .expect("second export session region");
    assert_eq!(click_overlay(&mut app, second.area.x, second.area.y), None);
    assert_eq!(
        app.export_flow
            .as_ref()
            .expect("export flow")
            .picker
            .selected,
        1
    );
    app.export_flow = None;

    app.dashboard = Some(DashboardState::new(DashboardTab::Plan));
    let usage = overlay_mouse_regions(app.layout.transcript, &app)
        .into_iter()
        .find(|region| region.press == UiMousePress::Dashboard(DashboardTab::Usage))
        .expect("usage tab region");
    assert_eq!(click_overlay(&mut app, usage.area.x, usage.area.y), None);
    assert_eq!(
        app.dashboard.as_ref().expect("dashboard").tab,
        DashboardTab::Usage
    );
    app.dashboard = None;

    app.help_dialog = Some(HelpDialogState::new(HelpTopic::Overview, "composer"));
    let whats_new = overlay_mouse_regions(app.layout.transcript, &app)
        .into_iter()
        .find(|region| region.press == UiMousePress::Help(HelpTopic::WhatsNew))
        .expect("what's new tab region");
    assert_eq!(
        click_overlay(&mut app, whats_new.area.x, whats_new.area.y),
        None
    );
    assert_eq!(
        app.help_dialog.as_ref().expect("help dialog").topic,
        HelpTopic::WhatsNew
    );
    assert!(!app.release_badge_visible);
    app.help_dialog = None;

    app.settings_dialog = Some(SettingsDialogState::new(
        &app.runtime_controls,
        &app.provider_choices,
        &app.preferences,
        false,
    ));
    let high_contrast = overlay_mouse_regions(app.layout.transcript, &app)
        .into_iter()
        .find(|region| region.press == UiMousePress::Settings(SettingsRow::HighContrast))
        .expect("high contrast row region");
    assert_eq!(
        click_overlay(&mut app, high_contrast.area.x, high_contrast.area.y),
        None
    );
    let settings = app.settings_dialog.as_ref().expect("settings dialog");
    assert_eq!(settings.selected_row, SettingsRow::HighContrast);
    assert!(settings.draft_preferences.high_contrast);
}

#[test]
fn narrow_wrapped_overlays_keep_mouse_targets_aligned_with_visible_rows() {
    let area = Rect::new(0, 0, 40, 8);
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.layout.transcript = area;

    let mut approval = ApprovalDialogState::new(ApprovalRequest {
        approval_id: golutra_core::ApprovalId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        tool_call_id: golutra_core::ToolCallId::new(),
        tool_name: "shell".to_owned(),
        resource: "cargo test --workspace --all-targets -- --test-threads=1".to_owned(),
        reason: "a long approval reason that wraps over several narrow terminal rows".to_owned(),
    });
    approval.select(ApprovalChoice::Deny);
    app.approval_dialog = Some(approval);
    let deny = overlay_mouse_regions(area, &app)
        .into_iter()
        .find(|region| region.press == UiMousePress::Approval(ApprovalChoice::Deny))
        .expect("visible deny region");
    assert!(area.intersects(deny.area));
    assert_eq!(
        click_overlay(&mut app, deny.area.x, deny.area.y),
        Some(UiMouseActivation::Approval(ApprovalChoice::Deny))
    );
    app.approval_dialog = None;

    let mut question = QuestionDialogState::new(UserQuestionRequest {
        question_id: golutra_core::QuestionId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        tool_call_id: golutra_core::ToolCallId::new(),
        questions: vec![golutra_core::UserQuestionPrompt {
            id: "strategy".to_owned(),
            header: "Implementation strategy".to_owned(),
            question: "Choose the implementation strategy that should be used for this change"
                .to_owned(),
            mode: golutra_core::UserQuestionMode::Single,
            options: vec![
                golutra_core::UserQuestionOption {
                    id: "first".to_owned(),
                    label: "Use the first deliberately long implementation option".to_owned(),
                    description: Some(
                        "This description also wraps on a narrow terminal".to_owned(),
                    ),
                },
                golutra_core::UserQuestionOption {
                    id: "second".to_owned(),
                    label: "Use the second deliberately long implementation option".to_owned(),
                    description: Some(
                        "Another wrapped option description that is intentionally taller than the visible dialog viewport "
                            .repeat(6),
                    ),
                },
            ],
        }],
    });
    question.focus(0, 1);
    app.question_dialog = Some(question);
    let second = overlay_mouse_regions(area, &app)
        .into_iter()
        .find(|region| {
            region.press
                == UiMousePress::QuestionOption {
                    question: 0,
                    option: 1,
                }
        })
        .expect("visible wrapped question option");
    assert_eq!(click_overlay(&mut app, second.area.x, second.area.y), None);
    assert!(
        app.question_dialog
            .as_ref()
            .expect("question dialog")
            .is_selected(1)
    );
    let submit = overlay_mouse_regions(area, &app)
        .into_iter()
        .find(|region| region.press == UiMousePress::QuestionSubmit)
        .expect("submit remains reachable below a tall final option");
    assert!(area.intersects(submit.area));
    app.question_dialog = None;

    let mut auth = AuthDialogState::new();
    auth.selected = AUTH_GROUP_ITEMS.len() - 1;
    app.auth_dialog = Some(auth);
    let quit = overlay_mouse_regions(area, &app)
        .into_iter()
        .find(|region| region.press == UiMousePress::Auth(AUTH_GROUP_ITEMS.len() - 1))
        .expect("visible wrapped auth option");
    assert_eq!(
        click_overlay(&mut app, quit.area.x, quit.area.y),
        Some(UiMouseActivation::AuthContinue)
    );
}

#[tokio::test]
async fn auth_manual_scrolling_overrides_selection_follow_until_selection_moves() {
    let transport = RuntimeTransport::in_memory().await.expect("transport");
    let area = Rect::new(0, 0, 32, 4);
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        Some(AuthDialogState::new()),
    );
    app.layout.transcript = area;
    app.auth_dialog.as_mut().expect("dialog").selected = AUTH_GROUP_ITEMS.len() - 1;

    handle_auth_dialog_key(
        KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .expect("home");
    assert_eq!(
        auth_scroll_offset(app.auth_dialog.as_ref().expect("dialog"), area),
        0
    );

    handle_auth_dialog_key(
        KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .expect("page down");
    let page_down = auth_scroll_offset(app.auth_dialog.as_ref().expect("dialog"), area);
    assert!(page_down > 0);

    handle_auth_dialog_key(
        KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .expect("end");
    let end = auth_scroll_offset(app.auth_dialog.as_ref().expect("dialog"), area);
    assert!(end >= page_down);

    handle_auth_dialog_key(
        KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .expect("page up");
    assert!(auth_scroll_offset(app.auth_dialog.as_ref().expect("dialog"), area) < end);

    handle_auth_dialog_key(
        KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .expect("selection move");
    let dialog = app.auth_dialog.as_ref().expect("dialog");
    assert!(!dialog.manual_scroll);
    assert!(auth_scroll_offset(dialog, area) > 0);
}

#[test]
fn help_scrolling_clamps_to_the_wrapped_content_height() {
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.layout.transcript = Rect::new(0, 0, 40, 8);
    app.help_dialog = Some(HelpDialogState::new(HelpTopic::Composer, "composer"));
    let max_scroll = help_scroll_max(
        app.help_dialog.as_ref().expect("help"),
        &app,
        app.layout.transcript,
    );
    assert!(max_scroll > 0);

    for _ in 0..50 {
        handle_help_dialog_key(
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
            &mut app,
        );
    }
    assert_eq!(app.help_dialog.as_ref().expect("help").scroll, max_scroll);

    let mut terminal = Terminal::new(TestBackend::new(40, 8)).expect("terminal");
    terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("draw final help page");
    assert!(terminal_buffer_text(&terminal).contains('@'));

    handle_help_dialog_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE), &mut app);
    assert_eq!(app.help_dialog.as_ref().expect("help").scroll, 0);
    handle_help_dialog_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), &mut app);
    assert_eq!(app.help_dialog.as_ref().expect("help").scroll, max_scroll);
}

#[test]
fn terminal_restore_failure_keeps_the_primary_runtime_error() {
    let error = combine_run_and_restore(
        Err(miette::miette!("runtime event loop failed")),
        Err(miette::miette!("stdout is closed")),
    )
    .expect_err("combined failure");
    let message = error.to_string();

    assert!(
        message.starts_with("runtime event loop failed"),
        "{message}"
    );
    assert!(
        message.contains("terminal restore failed: stdout is closed"),
        "{message}"
    );
}

#[test]
fn terminal_restore_guard_runs_during_unwind_and_can_be_disarmed() {
    let restore_count = std::cell::Cell::new(0_u8);
    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = TerminalRestoreGuard::new(|| restore_count.set(restore_count.get() + 1));
        panic!("simulated event-loop panic");
    }));

    assert!(unwind.is_err());
    assert_eq!(restore_count.get(), 1);

    let mut guard = TerminalRestoreGuard::new(|| restore_count.set(restore_count.get() + 1));
    guard.disarm();
    drop(guard);
    assert_eq!(restore_count.get(), 1);
}

#[test]
fn panic_report_is_emitted_after_terminal_restoration() {
    let operations = std::cell::RefCell::new(Vec::new());

    restore_before_panic_report(
        || operations.borrow_mut().push("restore"),
        || operations.borrow_mut().push("report"),
    );

    assert_eq!(*operations.borrow(), ["restore", "report"]);
}

#[test]
fn help_and_settings_keep_selected_content_visible_at_narrow_and_wide_widths() {
    for width in [40, 120] {
        let mut app = TuiApp::new(
            ThreadId::new(),
            SessionId::new(),
            None,
            false,
            "ready (mock)".to_owned(),
            None,
        );
        app.help_dialog = Some(HelpDialogState::new(HelpTopic::Composer, "composer"));
        let mut terminal = Terminal::new(TestBackend::new(width, 18)).expect("help terminal");
        terminal
            .draw(|frame| draw_ui(frame, &mut app))
            .expect("draw help");
        let rendered = terminal_buffer_text(&terminal);
        assert!(rendered.contains("Help"));
        assert!(rendered.contains("Enter"));
        assert!(
            overlay_mouse_regions(app.layout.transcript, &app)
                .iter()
                .any(|region| region.press == UiMousePress::Help(HelpTopic::WhatsNew))
        );

        app.help_dialog = None;
        let mut settings = SettingsDialogState::new(
            &app.runtime_controls,
            &app.provider_choices,
            &app.preferences,
            false,
        );
        settings.selected_row = SettingsRow::ScreenReader;
        app.settings_dialog = Some(settings);
        terminal
            .draw(|frame| draw_ui(frame, &mut app))
            .expect("draw settings");
        let rendered = terminal_buffer_text(&terminal);
        assert!(rendered.contains("Settings"));
        assert!(rendered.contains("Screen reader symbols"));
    }
}

#[test]
fn semantic_theme_and_screen_reader_preferences_reach_the_composer() {
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.preferences.theme = ColorTheme::Amber;
    app.preferences.screen_reader = true;
    let mut terminal = Terminal::new(TestBackend::new(60, 4)).expect("terminal");

    terminal
        .draw(|frame| draw_bottom_pane(frame, frame.area(), &app))
        .expect("draw amber composer");

    let prefix = terminal
        .backend()
        .buffer()
        .cell((0, 1))
        .expect("composer prefix");
    assert_eq!(prefix.symbol(), ">");
    assert_eq!(prefix.fg, ratatui::style::Color::Yellow);

    app.preferences.high_contrast = true;
    terminal
        .draw(|frame| draw_bottom_pane(frame, frame.area(), &app))
        .expect("draw high contrast composer");
    assert_eq!(
        terminal
            .backend()
            .buffer()
            .cell((0, 1))
            .expect("high contrast prefix")
            .fg,
        ratatui::style::Color::LightCyan
    );
}

#[tokio::test]
async fn malformed_preferences_can_be_repaired_from_the_settings_surface() {
    let home = tempfile::tempdir().expect("home");
    let _guard = env_lock_guard().await;
    let previous_home = std::env::var("GOLUTRA_HOME").ok();
    unsafe {
        std::env::set_var("GOLUTRA_HOME", home.path());
    }
    let path = home.path().join("tui.json");
    std::fs::write(&path, "{not-json").expect("malformed preferences");

    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    )
    .with_loaded_preferences();
    assert_eq!(app.preferences_path.as_deref(), Some(path.as_path()));
    assert!(app.status_message.contains("were not loaded"));

    app.preferences.theme = ColorTheme::Monochrome;
    app.persist_preferences();
    assert_eq!(
        TuiPreferences::load_from(&path)
            .expect("repaired preferences")
            .theme,
        ColorTheme::Monochrome
    );

    unsafe {
        match previous_home {
            Some(previous_home) => std::env::set_var("GOLUTRA_HOME", previous_home),
            None => std::env::remove_var("GOLUTRA_HOME"),
        }
    }
}

fn click_overlay(app: &mut TuiApp, column: u16, row: u16) -> Option<UiMouseActivation> {
    let down = handle_mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        },
        app,
    );
    let up = handle_mouse(
        MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        },
        app,
    );
    up.or(down)
}

fn resume_item(title: &str) -> ResumeThreadItem {
    ResumeThreadItem {
        thread_id: ThreadId::new(),
        session_id: SessionId::new(),
        title: title.to_owned(),
        preview: format!("{title} preview"),
    }
}

#[tokio::test]
async fn debug_page_keys_leave_history_navigation_to_the_terminal() {
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
    let before = terminal_buffer_text(&terminal);
    let generation = app.history_replay_generation;

    handle_key(
        KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .expect("ignore debug page key");

    terminal
        .draw(|frame| draw_ui(frame, &mut app))
        .expect("redraw");
    let after = terminal_buffer_text(&terminal);
    assert!(!after.contains("▸ facts"));
    assert!(!after.contains("▾ facts"));
    assert_eq!(after, before);
    assert_eq!(app.transcript_scroll.offset_from_bottom, 0);
    assert_eq!(app.history_replay_generation, generation);
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
    app.resume_picker = Some(ResumePickerState::new(items.clone()));
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
        picker: ResumePickerState::new(items),
        step: ExportFlowStep::SelectSession,
        range_input: ComposerInput::from_text("1"),
        destination_input: ComposerInput::default(),
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
async fn session_picker_actions_update_runtime_and_visible_items() {
    let transport = RuntimeTransport::in_memory().await.expect("transport");
    let current_session_id = transport.default_session_id();
    let current_thread_id = ThreadId::new();
    let archive_session_id = SessionId::new();
    let archive_thread_id = ThreadId::new();
    let delete_session_id = SessionId::new();
    let delete_thread_id = ThreadId::new();
    for (session_id, thread_id, prompt) in [
        (current_session_id, current_thread_id, "current"),
        (archive_session_id, archive_thread_id, "archive target"),
        (delete_session_id, delete_thread_id, "delete target"),
    ] {
        let ack = transport
            .send_command(session_command(
                session_id,
                SessionCommandKind::Create,
                json!({"_thread_id": thread_id, "prompt": prompt}),
            ))
            .await
            .expect("create session");
        assert!(ack.accepted);
    }
    let mut app = TuiApp::new(
        current_thread_id,
        current_session_id,
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.open_resume_picker(&transport)
        .await
        .expect("open session picker");

    {
        let picker = app.resume_picker.as_mut().expect("resume picker");
        picker.selected = picker
            .items
            .iter()
            .position(|item| item.thread_id == archive_thread_id)
            .expect("archive target");
        assert!(picker.begin_action(SessionPickerAction::Rename));
        picker.action_input.set_text("renamed target");
    }
    app.apply_session_picker_action(&transport)
        .await
        .expect("rename target");
    assert!(
        app.last_control_ack
            .as_ref()
            .is_some_and(|ack| ack.accepted)
    );
    assert!(
        app.resume_picker
            .as_ref()
            .expect("resume picker")
            .items
            .iter()
            .any(|item| item.thread_id == archive_thread_id && item.title == "renamed target")
    );

    {
        let picker = app.resume_picker.as_mut().expect("resume picker");
        assert!(picker.begin_action(SessionPickerAction::Archive));
    }
    app.apply_session_picker_action(&transport)
        .await
        .expect("archive target");
    assert!(
        app.resume_picker
            .as_ref()
            .expect("resume picker")
            .items
            .iter()
            .all(|item| item.thread_id != archive_thread_id)
    );

    {
        let picker = app.resume_picker.as_mut().expect("resume picker");
        picker.selected = picker
            .items
            .iter()
            .position(|item| item.thread_id == delete_thread_id)
            .expect("delete target");
        assert!(picker.begin_action(SessionPickerAction::Delete));
    }
    app.apply_session_picker_action(&transport)
        .await
        .expect("delete target");
    assert!(
        app.resume_picker
            .as_ref()
            .expect("resume picker")
            .items
            .iter()
            .all(|item| item.thread_id != delete_thread_id)
    );
    let threads = transport.list_threads(50).await.expect("runtime threads");
    assert!(threads.iter().all(|thread| {
        thread.thread_id != archive_thread_id && thread.thread_id != delete_thread_id
    }));
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
        picker: ResumePickerState::new(vec![ResumeThreadItem {
            thread_id,
            session_id,
            title: "export fixture".to_owned(),
            preview: "export this session".to_owned(),
        }]),
        step: ExportFlowStep::Review,
        range_input: ComposerInput::from_text("1"),
        destination_input: ComposerInput::from_text(destination.display().to_string()),
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
    app.resume_picker = Some(ResumePickerState::new(Vec::new()));
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
