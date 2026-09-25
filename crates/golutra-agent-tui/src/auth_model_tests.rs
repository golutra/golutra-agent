//! 模型发现的网络边界与向导交互回归；本地接口可控延迟，不使用真实凭据。

use super::*;
use tokio::sync::oneshot;

async fn catalog_server() -> (
    String,
    oneshot::Receiver<String>,
    oneshot::Sender<&'static str>,
    JoinHandle<()>,
) {
    setup_server("application/json").await
}

async fn setup_server(
    content_type: &'static str,
) -> (
    String,
    oneshot::Receiver<String>,
    oneshot::Sender<&'static str>,
    JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (request_tx, request_rx) = oneshot::channel();
    let (response_tx, response_rx) = oneshot::channel::<&'static str>();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let mut chunk = [0; 2048];
            let n = socket.read(&mut chunk).await.unwrap();
            assert!(n > 0);
            request.extend_from_slice(&chunk[..n]);
        }
        let _ = request_tx.send(String::from_utf8(request).unwrap());
        if let Ok(body) = response_rx.await {
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    (base, request_rx, response_tx, task)
}

fn catalog_app(base: &str) -> TuiApp {
    catalog_app_for(base, OFFICIAL_PROVIDER_PRESET)
}

fn detection_app(base: &str) -> TuiApp {
    let mut app = catalog_app(base);
    let dialog = app.auth_dialog.as_mut().unwrap();
    dialog.step = AuthDialogStep::AdvancedConfig;
    dialog.model = "gpt-test".to_owned();
    app
}

async fn finish_detection(app: &mut TuiApp) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while app.auth_protocol_detection.is_some() {
            app.poll_auth_protocol_detection().await;
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn auth_protocol_detection_uses_generation_and_then_opens_offline_review() {
    let _guard = env_lock_guard().await;
    let transport = RuntimeTransport::in_memory().await.unwrap();
    let (base, received, release, server) = setup_server("text/event-stream").await;
    let mut app = detection_app(&base);
    advance_auth_dialog(&mut app, &transport).await.unwrap();
    assert!(app.auth_protocol_detection.is_some());
    assert!(app.auth_dialog.as_ref().unwrap().review.is_none());
    let request = tokio::time::timeout(Duration::from_secs(5), received)
        .await
        .unwrap()
        .unwrap();
    assert!(request.starts_with("POST /v1/responses"), "{request}");
    assert!(
        auth_advanced_config_lines(app.auth_dialog.as_ref().unwrap())
            .iter()
            .any(|line| line.to_string().contains("Detecting provider protocol"))
    );
    release
        .send(include_str!(
            "../../golutra-agent-llm/tests/fixtures/openai-responses/text-response.sse"
        ))
        .unwrap();
    finish_detection(&mut app).await;
    server.await.unwrap();
    let dialog = app.auth_dialog.as_ref().unwrap();
    assert_eq!(dialog.step, AuthDialogStep::Review, "{:?}", dialog.error);
    assert_eq!(dialog.review.as_ref().unwrap().protocol, "openai-responses");
    assert!(app.auth_operation.is_none());

    // 上游已关闭；从确认页返回再继续必须复用成功结果，不发第二次请求。
    handle_auth_dialog_key(
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .unwrap();
    advance_auth_dialog(&mut app, &transport).await.unwrap();
    assert!(app.auth_protocol_detection.is_none());
    assert_eq!(
        app.auth_dialog.as_ref().unwrap().step,
        AuthDialogStep::Review
    );

    let original = app.auth_dialog.as_ref().unwrap().clone();
    for field in ["base", "key", "model", "effort", "tokens", "headers"] {
        let mut changed = original.clone();
        changed.go_back();
        match field {
            "base" => changed.base_url.push_str("/other"),
            "key" => changed.api_key.push_str("-changed"),
            "model" => changed.model.push_str("-changed"),
            "effort" => changed.reasoning_effort = Some(ProviderReasoningEffort::High),
            "tokens" => changed.max_tokens = "2048".to_owned(),
            "headers" => changed.custom_headers = "X-Client=changed".to_owned(),
            _ => unreachable!(),
        }
        app.auth_dialog = Some(changed);
        advance_auth_dialog(&mut app, &transport).await.unwrap();
        assert!(
            app.auth_protocol_detection.is_some(),
            "{field} must invalidate detection"
        );
        assert!(
            app.auth_dialog
                .as_ref()
                .unwrap()
                .successful_protocol_detection
                .is_none()
        );
        app.cancel_auth_protocol_detection();
    }
}

#[tokio::test]
async fn auth_protocol_detection_cache_tracks_environment_key_and_header_values() {
    let _guard = env_lock_guard().await;
    let variables = [
        "GOLUTRA_AGENT_TEST_DETECTION_KEY",
        "GOLUTRA_AGENT_TEST_DETECTION_HEADER",
    ];
    let previous = variables.map(std::env::var_os);
    for variable in variables {
        unsafe {
            std::env::set_var(variable, "initial-test-secret");
        }
    }
    let (base, received, release, server) = setup_server("text/event-stream").await;
    let transport = RuntimeTransport::in_memory().await.unwrap();
    let mut app = detection_app(&base);
    let dialog = app.auth_dialog.as_mut().unwrap();
    dialog.credential_store = AuthCredentialStore::Environment;
    dialog.api_key.clear();
    dialog.api_key_env = variables[0].to_owned();
    dialog.custom_headers = format!("X-Api-Key=@{}", variables[1]);
    advance_auth_dialog(&mut app, &transport).await.unwrap();
    let request = tokio::time::timeout(Duration::from_secs(5), received)
        .await
        .unwrap()
        .unwrap();
    assert!(request.contains("Bearer initial-test-secret"));
    assert!(
        request
            .to_ascii_lowercase()
            .contains("x-api-key: initial-test-secret")
    );
    release
        .send(include_str!(
            "../../golutra-agent-llm/tests/fixtures/openai-responses/text-response.sse"
        ))
        .unwrap();
    finish_detection(&mut app).await;
    server.await.unwrap();
    let dialog = app.auth_dialog.as_mut().unwrap();
    assert_eq!(dialog.step, AuthDialogStep::Review, "{:?}", dialog.error);
    assert!(!format!("{:?}", dialog.successful_protocol_detection).contains("initial-test-secret"));
    dialog.go_back();
    advance_auth_dialog(&mut app, &transport).await.unwrap();
    assert!(app.auth_protocol_detection.is_none());
    assert_eq!(
        app.auth_dialog.as_ref().unwrap().step,
        AuthDialogStep::Review
    );
    let cached = app.auth_dialog.as_ref().unwrap().clone();
    for variable in variables {
        app.auth_dialog = Some(cached.clone());
        app.auth_dialog.as_mut().unwrap().go_back();
        unsafe {
            std::env::set_var(variable, "replacement-test-secret");
        }
        advance_auth_dialog(&mut app, &transport).await.unwrap();
        assert!(app.auth_protocol_detection.is_some(), "{variable}");
        // 请求发出后环境又变化，即使迟到结果成功也不能认证新值。
        let pending = app.auth_protocol_detection.as_mut().unwrap();
        pending.task.abort();
        pending.task = tokio::spawn(async { Ok(ProviderProtocol::OpenAiResponses) });
        unsafe {
            std::env::set_var(variable, "initial-test-secret");
        }
        finish_detection(&mut app).await;
        let dialog = app.auth_dialog.as_ref().unwrap();
        assert!(dialog.successful_protocol_detection.is_none());
        assert!(dialog.review.is_none());
        assert!(
            dialog
                .error
                .as_deref()
                .unwrap()
                .contains("settings changed")
        );
    }
    for (variable, value) in variables.into_iter().zip(previous) {
        unsafe {
            match value {
                Some(value) => std::env::set_var(variable, value),
                None => std::env::remove_var(variable),
            }
        }
    }
}

#[tokio::test]
async fn auth_protocol_detection_failure_allows_retry_and_manual_offline_review() {
    let _guard = env_lock_guard().await;
    let transport = RuntimeTransport::in_memory().await.unwrap();
    let mut app = detection_app("http://127.0.0.1:1");
    advance_auth_dialog(&mut app, &transport).await.unwrap();
    let pending = app.auth_protocol_detection.as_mut().unwrap();
    pending.task.abort();
    pending.task =
        tokio::spawn(async { Err("Detection timed out; protocol remains unconfirmed".to_owned()) });
    finish_detection(&mut app).await;
    let dialog = app.auth_dialog.as_ref().unwrap();
    assert!(dialog.successful_protocol_detection.is_none());
    let text = auth_advanced_config_lines(dialog)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("unconfirmed"));
    assert!(text.contains("Continue to retry"));
    assert!(text.contains("Ctrl+P"));
    assert!(text.contains("save without detection"));
    advance_auth_dialog(&mut app, &transport).await.unwrap();
    assert!(app.auth_protocol_detection.is_some());
    app.cancel_auth_protocol_detection();
    let failed = app.auth_dialog.as_ref().unwrap().clone();
    for environment in [false, true] {
        app.auth_dialog = Some(failed.clone());
        if environment {
            let dialog = app.auth_dialog.as_mut().unwrap();
            dialog.credential_store = AuthCredentialStore::Environment;
            dialog.api_key_env = "GOLUTRA_AGENT_TEST_MANUAL_PROTOCOL_KEY".to_owned();
            dialog.api_key.clear();
        }
        handle_auth_dialog_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            &mut app,
            &transport,
        )
        .await
        .unwrap();
        assert_eq!(
            app.auth_dialog.as_ref().unwrap().step,
            AuthDialogStep::Protocol
        );
        for expected in [
            if environment {
                AuthDialogStep::EnvKey
            } else {
                AuthDialogStep::ApiKey
            },
            AuthDialogStep::Model,
            AuthDialogStep::AdvancedConfig,
            AuthDialogStep::Review,
        ] {
            advance_auth_dialog(&mut app, &transport).await.unwrap();
            assert_eq!(app.auth_dialog.as_ref().unwrap().step, expected);
            assert!(app.auth_protocol_detection.is_none());
        }
        assert!(!app.auth_dialog.as_ref().unwrap().automatic_protocol);
    }
}

#[tokio::test]
async fn auth_protocol_detection_cancels_without_applying_old_result_or_editing_draft() {
    let _guard = env_lock_guard().await;
    let transport = RuntimeTransport::in_memory().await.unwrap();
    let (base, received, release, server) = setup_server("text/event-stream").await;
    let mut app = detection_app(&base);
    advance_auth_dialog(&mut app, &transport).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), received)
        .await
        .unwrap()
        .unwrap();
    handle_paste("unexpected", &mut app);
    handle_auth_dialog_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .unwrap();
    assert_eq!(app.auth_dialog.as_ref().unwrap().model, "gpt-test");
    handle_auth_dialog_key(
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .unwrap();
    assert!(app.auth_protocol_detection.is_none());
    assert_eq!(
        app.auth_dialog.as_ref().unwrap().step,
        AuthDialogStep::AdvancedConfig
    );
    let _ = release.send(include_str!(
        "../../golutra-agent-llm/tests/fixtures/openai-responses/text-response.sse"
    ));
    server.await.unwrap();
    assert!(!app.poll_auth_protocol_detection().await);
    assert!(app.auth_dialog.as_ref().unwrap().review.is_none());
}

#[tokio::test]
async fn auth_protocol_detection_discards_result_after_dialog_replacement() {
    let mut app = detection_app("http://127.0.0.1:1");
    let id = Uuid::new_v4();
    app.auth_dialog.as_mut().unwrap().protocol_detection = ModelDiscoveryState::Loading(id);
    app.auth_protocol_detection = Some(PendingProtocolDetection {
        id,
        fingerprint: ProtocolDetectionFingerprint([0; 32]),
        task: tokio::spawn(async { Ok(ProviderProtocol::Anthropic) }),
    });
    app.auth_dialog = Some(AuthDialogState::new());
    assert!(app.poll_auth_protocol_detection().await);
    assert!(app.auth_protocol_detection.is_none());
    assert_eq!(
        app.auth_dialog.as_ref().unwrap().step,
        AuthDialogStep::GroupChoice
    );
    assert!(app.auth_dialog.as_ref().unwrap().review.is_none());
}

fn catalog_app_for(base: &str, provider: AuthProviderPreset) -> TuiApp {
    let mut dialog = AuthDialogState::new();
    dialog.select_provider(provider);
    dialog.step = AuthDialogStep::ApiKey;
    dialog.base_url = base.to_owned();
    dialog.api_key = "fake-catalog-key".to_owned();
    TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "setup".to_owned(),
        Some(dialog),
    )
}

async fn finish_catalog(app: &mut TuiApp) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while app.auth_model_discovery.is_some() {
            app.poll_auth_model_discovery().await;
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("catalog completed");
}

fn model_text(app: &TuiApp) -> String {
    auth_model_lines(app.auth_dialog.as_ref().unwrap())
        .iter()
        .map(Line::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn auth_catalog_loads_after_key_and_selects_upstream_model() {
    let _guard = env_lock_guard().await;
    for provider in [OFFICIAL_PROVIDER_PRESET, CUSTOM_PROVIDER_PRESET] {
        let (base, received, release, server) = catalog_server().await;
        let transport = RuntimeTransport::in_memory().await.unwrap();
        let mut app = catalog_app_for(&base, provider);
        assert!(app.auth_dialog.as_ref().unwrap().models.is_empty());
        assert!(app.auth_dialog.as_ref().unwrap().model.is_empty());
        tokio::time::timeout(
            Duration::from_millis(500),
            advance_auth_dialog(&mut app, &transport),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            app.auth_dialog.as_ref().unwrap().step,
            AuthDialogStep::Model
        );
        assert!(model_text(&app).contains("Loading models"));
        let request = tokio::time::timeout(Duration::from_secs(12), received)
            .await
            .unwrap()
            .unwrap();
        assert!(request.starts_with("GET /v1/models HTTP/1.1"));
        assert!(request.contains("Bearer fake-catalog-key"));
        assert!(
            !app.poll_auth_model_discovery().await,
            "pending lookup must not force continuous redraws"
        );
        release
            .send(r#"{"data":[{"id":"upstream-new"},{"id":"another-model"}]}"#)
            .unwrap();
        finish_catalog(&mut app).await;
        server.await.unwrap();
        assert!(model_text(&app).contains("from provider"));
        assert!(model_text(&app).contains("upstream-new"));
        assert!(!model_text(&app).contains("gpt-test"));
        assert_eq!(app.auth_dialog.as_ref().unwrap().selected, 0);
        assert!(app.auth_dialog.as_ref().unwrap().model.is_empty());
        assert!(
            model_text(&app).find("Custom model").unwrap()
                < model_text(&app).find("upstream-new").unwrap()
        );
        handle_auth_dialog_key(
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &mut app,
            &transport,
        )
        .await
        .unwrap();
        handle_auth_dialog_key(
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &mut app,
            &transport,
        )
        .await
        .unwrap();
        advance_auth_dialog(&mut app, &transport).await.unwrap();
        let dialog = app.auth_dialog.as_mut().unwrap();
        assert_eq!(dialog.step, AuthDialogStep::AdvancedConfig);
        assert_eq!(dialog.model, "another-model");
        // 返回模型页复用本次结果，不额外请求，也不丢失选择。
        dialog.go_back();
        assert_eq!(dialog.selected_recommended_model(), Some("another-model"));
        assert!(app.auth_model_discovery.is_none());
    }
}

#[tokio::test]
async fn auth_catalog_does_not_replace_manual_input_or_paste_when_response_arrives() {
    let _guard = env_lock_guard().await;
    for (provider, paste) in [
        (OFFICIAL_PROVIDER_PRESET, false),
        (OFFICIAL_PROVIDER_PRESET, true),
        (CUSTOM_PROVIDER_PRESET, false),
        (CUSTOM_PROVIDER_PRESET, true),
    ] {
        let (base, received, release, server) = catalog_server().await;
        let transport = RuntimeTransport::in_memory().await.unwrap();
        let mut app = catalog_app_for(&base, provider);
        advance_auth_dialog(&mut app, &transport).await.unwrap();
        let dialog = app.auth_dialog.as_ref().unwrap();
        assert_eq!(dialog.selected, 0);
        assert!(dialog.model.is_empty());
        assert!(dialog.models.is_empty());
        assert!(model_text(&app).contains("Custom model"));
        tokio::time::timeout(Duration::from_secs(12), received)
            .await
            .unwrap()
            .unwrap();
        if paste {
            handle_paste("custom-model", &mut app);
        } else {
            for character in "custom-model".chars() {
                handle_auth_dialog_key(
                    KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
                    &mut app,
                    &transport,
                )
                .await
                .unwrap();
            }
        }
        release.send(r#"{"data":[{"id":"custom-model"}]}"#).unwrap();
        finish_catalog(&mut app).await;
        server.await.unwrap();
        let dialog = app.auth_dialog.as_ref().unwrap();
        assert!(dialog.is_custom_model_selected());
        assert_eq!(dialog.model, "custom-model");
        assert_eq!(dialog.models, ["custom-model"]);
        assert!(
            model_text(&app).find("Custom model").unwrap()
                < model_text(&app).find("from provider").unwrap()
        );
        // 目录包含刚输入的完整 ID，也不能在继续键入后缀时把它清空。
        handle_auth_dialog_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            &mut app,
            &transport,
        )
        .await
        .unwrap();
        advance_auth_dialog(&mut app, &transport).await.unwrap();
        assert_eq!(app.auth_dialog.as_ref().unwrap().model, "custom-modelx");
    }
}

#[tokio::test]
async fn auth_catalog_discards_old_results_after_back_new_key_or_reopen() {
    let _guard = env_lock_guard().await;
    for reopen in [false, true] {
        let (base, received, release, server) = catalog_server().await;
        let transport = RuntimeTransport::in_memory().await.unwrap();
        let mut app = catalog_app(&base);
        advance_auth_dialog(&mut app, &transport).await.unwrap();
        tokio::time::timeout(Duration::from_secs(12), received)
            .await
            .unwrap()
            .unwrap();
        let old_task = app
            .auth_model_discovery
            .as_ref()
            .unwrap()
            .task
            .abort_handle();
        if reopen {
            app.auth_dialog = Some(AuthDialogState::new());
        } else {
            handle_auth_dialog_key(
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                &mut app,
                &transport,
            )
            .await
            .unwrap();
            app.auth_dialog.as_mut().unwrap().api_key = "replacement-key".to_owned();
        }
        release.send(r#"{"data":[{"id":"stale-model"}]}"#).unwrap();
        finish_catalog(&mut app).await;
        server.await.unwrap();
        assert!(app.auth_dialog.as_ref().unwrap().models.is_empty());
        tokio::task::yield_now().await;
        assert!(old_task.is_finished());
        if !reopen {
            let (base, received, release, server) = catalog_server().await;
            app.auth_dialog.as_mut().unwrap().base_url = base;
            advance_auth_dialog(&mut app, &transport).await.unwrap();
            let request = tokio::time::timeout(Duration::from_secs(12), received)
                .await
                .unwrap()
                .unwrap();
            assert!(request.contains("Bearer replacement-key"));
            release.send(r#"{"data":[{"id":"fresh-model"}]}"#).unwrap();
            finish_catalog(&mut app).await;
            server.await.unwrap();
            assert_eq!(app.auth_dialog.as_ref().unwrap().models, ["fresh-model"]);
        }
    }
}

#[tokio::test]
async fn auth_catalog_empty_or_invalid_response_allows_manual_entry() {
    let _guard = env_lock_guard().await;
    for (provider, body, message) in [
        (
            OFFICIAL_PROVIDER_PRESET,
            r#"{"data":[]}"#,
            "returned no models",
        ),
        (
            OFFICIAL_PROVIDER_PRESET,
            "<html>gateway</html>",
            "not valid models JSON",
        ),
        (
            CUSTOM_PROVIDER_PRESET,
            r#"{"data":[]}"#,
            "returned no models",
        ),
        (
            CUSTOM_PROVIDER_PRESET,
            "<html>gateway</html>",
            "not valid models JSON",
        ),
    ] {
        let (base, received, release, server) = catalog_server().await;
        let transport = RuntimeTransport::in_memory().await.unwrap();
        let mut app = catalog_app_for(&base, provider);
        advance_auth_dialog(&mut app, &transport).await.unwrap();
        tokio::time::timeout(Duration::from_secs(12), received)
            .await
            .unwrap()
            .unwrap();
        release.send(body).unwrap();
        finish_catalog(&mut app).await;
        server.await.unwrap();
        assert!(model_text(&app).contains(message));
        handle_paste("manual-model", &mut app);
        advance_auth_dialog(&mut app, &transport).await.unwrap();
        assert_eq!(
            app.auth_dialog.as_ref().unwrap().step,
            AuthDialogStep::AdvancedConfig
        );
    }
}

#[tokio::test]
async fn auth_catalog_manual_continue_cancels_pending_lookup() {
    let _guard = env_lock_guard().await;
    for provider in [OFFICIAL_PROVIDER_PRESET, CUSTOM_PROVIDER_PRESET] {
        let (base, _received, _release, server) = catalog_server().await;
        let transport = RuntimeTransport::in_memory().await.unwrap();
        let mut app = catalog_app_for(&base, provider);
        advance_auth_dialog(&mut app, &transport).await.unwrap();
        assert!(app.auth_model_discovery.is_some());
        // 即使客户端尚未初始化或网络尚未连通，也必须能立即继续到离线确认页。
        handle_paste("manual-model", &mut app);
        advance_auth_dialog(&mut app, &transport).await.unwrap();
        assert_eq!(
            app.auth_dialog.as_ref().unwrap().step,
            AuthDialogStep::AdvancedConfig
        );
        assert!(app.auth_model_discovery.is_none());
        // 手动模式可跳过联网探测；自动模式另有独立探测和取消回归。
        app.auth_dialog.as_mut().unwrap().automatic_protocol = false;
        advance_auth_dialog(&mut app, &transport).await.unwrap();
        assert_eq!(
            app.auth_dialog.as_ref().unwrap().step,
            AuthDialogStep::Review
        );
        server.abort();
        let _ = server.await;
        let dialog = app.auth_dialog.as_mut().unwrap();
        dialog.go_back();
        dialog.go_back();
        assert_eq!(dialog.model, "manual-model");
        assert!(dialog.models.is_empty());
        assert!(!model_text(&app).contains("Loading models"));
    }
}

#[test]
fn auth_catalog_long_list_keeps_selected_model_and_custom_input_visible() {
    let mut app = catalog_app("http://127.0.0.1:1");
    let dialog = app.auth_dialog.as_mut().unwrap();
    dialog.step = AuthDialogStep::Model;
    dialog.model_discovery = ModelDiscoveryState::Ready;
    dialog.models = (0..100)
        .map(|index| format!("upstream-model-{index:03}"))
        .collect();
    for (index, expected) in [(100, "upstream-model-099"), (0, "Custom model")] {
        app.auth_dialog
            .as_mut()
            .unwrap()
            .set_interactive_selection(index);
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        terminal
            .draw(|frame| {
                draw_auth_dialog(frame, frame.area(), app.auth_dialog.as_ref().unwrap(), &app);
            })
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            text.contains(expected),
            "{expected} missing from selected viewport: {text}"
        );
    }
}

#[tokio::test]
async fn auth_catalog_uses_environment_value_without_copying_it_into_dialog_or_login() {
    let _guard = env_lock_guard().await;
    let variable = "GOLUTRA_AGENT_TEST_MODEL_CATALOG_KEY";
    let previous = std::env::var_os(variable);
    unsafe {
        std::env::set_var(variable, "env-catalog-secret");
    }
    let (base, received, release, server) = catalog_server().await;
    let transport = RuntimeTransport::in_memory().await.unwrap();
    let mut app = catalog_app(&base);
    let dialog = app.auth_dialog.as_mut().unwrap();
    dialog.toggle_credential_input();
    dialog.api_key_env = variable.to_owned();
    advance_auth_dialog(&mut app, &transport).await.unwrap();
    let request = tokio::time::timeout(Duration::from_secs(12), received)
        .await
        .unwrap()
        .unwrap();
    release.send(r#"{"data":[{"id":"env-model"}]}"#).unwrap();
    finish_catalog(&mut app).await;
    server.await.unwrap();
    handle_auth_dialog_key(
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .unwrap();
    advance_auth_dialog(&mut app, &transport).await.unwrap();
    let dialog = app.auth_dialog.as_ref().unwrap();
    assert!(request.contains("Bearer env-catalog-secret"));
    assert!(dialog.api_key.is_empty());
    let (reference, secret) = credential_for_login(&auth_login(dialog).unwrap()).unwrap();
    assert!(matches!(
        reference.source,
        CredentialSource::Environment { .. }
    ));
    assert!(secret.is_none());
    unsafe {
        std::env::remove_var(variable);
    }
    app.auth_dialog.as_mut().unwrap().go_back();
    app.auth_dialog.as_mut().unwrap().go_back();
    advance_auth_dialog(&mut app, &transport).await.unwrap();
    assert!(app.auth_model_discovery.is_none());
    assert!(model_text(&app).contains("unavailable in the current environment"));
    unsafe {
        if let Some(value) = previous {
            std::env::set_var(variable, value);
        }
    }
}
