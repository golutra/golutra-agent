//! 模型发现的网络边界与向导交互回归；本地接口可控延迟，不使用真实凭据。

use super::*;
use tokio::sync::oneshot;

async fn catalog_server() -> (
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
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
