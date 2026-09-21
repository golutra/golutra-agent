//! 高级配置的真实键盘分发与输入框渲染回归，确保修改字段不会意外继续向导。

use super::*;

fn advanced_app() -> TuiApp {
    let mut dialog = AuthDialogState::new();
    dialog.select_provider(CUSTOM_PROVIDER_PRESET);
    dialog.step = AuthDialogStep::AdvancedConfig;
    dialog.base_url = "http://127.0.0.1:9/v1".to_owned();
    dialog.model = "test-model".to_owned();
    dialog.api_key = "fake-key".to_owned();
    TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "setup".to_owned(),
        Some(dialog),
    )
}

async fn press(app: &mut TuiApp, transport: &RuntimeTransport, code: KeyCode) {
    handle_key(KeyEvent::new(code, KeyModifiers::NONE), app, transport)
        .await
        .unwrap();
}

#[test]
fn auth_advanced_continue_is_first_and_selected_by_default() {
    let app = advanced_app();
    let dialog = app.auth_dialog.as_ref().unwrap();
    let rows = auth_advanced_config_lines(dialog)
        .iter()
        .map(Line::to_string)
        .collect::<Vec<_>>();
    assert!(rows[2].contains("> 1 Continue"), "{rows:?}");
    assert!(rows[3].contains("2 Thinking"));
    assert!(rows[7].contains("6 Custom headers"));
    assert_eq!(auth_composer_line(dialog), "Continue to review");
}

#[tokio::test]
async fn auth_advanced_enter_and_arrows_change_options_without_leaving_page() {
    let transport = RuntimeTransport::in_memory().await.unwrap();
    let mut app = advanced_app();
    press(&mut app, &transport, KeyCode::Down).await;
    press(&mut app, &transport, KeyCode::Enter).await;
    assert!(app.auth_dialog.as_ref().unwrap().enable_thinking);
    press(&mut app, &transport, KeyCode::Left).await;
    assert!(!app.auth_dialog.as_ref().unwrap().enable_thinking);
    press(&mut app, &transport, KeyCode::Down).await;
    for label in ["low", "medium", "high", "xhigh", "max", "ultra", "default"] {
        press(&mut app, &transport, KeyCode::Right).await;
        assert_eq!(
            reasoning_effort_label(app.auth_dialog.as_ref().unwrap().reasoning_effort),
            label
        );
    }
    for label in ["ultra", "max", "xhigh", "high", "medium", "low", "default"] {
        press(&mut app, &transport, KeyCode::Left).await;
        assert_eq!(
            reasoning_effort_label(app.auth_dialog.as_ref().unwrap().reasoning_effort),
            label
        );
    }
    press(&mut app, &transport, KeyCode::Enter).await;
    let dialog = app.auth_dialog.as_ref().unwrap();
    assert_eq!(dialog.reasoning_effort, Some(ProviderReasoningEffort::Low));
    assert_eq!(dialog.step, AuthDialogStep::AdvancedConfig);
    assert!(dialog.review.is_none());
}

#[tokio::test]
async fn auth_advanced_text_edit_keeps_cursor_paste_and_vim_letters_local() {
    let transport = RuntimeTransport::in_memory().await.unwrap();
    let mut app = advanced_app();
    for _ in 0..3 {
        press(&mut app, &transport, KeyCode::Down).await;
    }
    // 导航状态只负责选择；必须先 Enter，避免浏览时误改已有配置。
    press(&mut app, &transport, KeyCode::Char('9')).await;
    handle_paste("999", &mut app);
    assert!(
        app.auth_dialog
            .as_ref()
            .unwrap()
            .context_window_size
            .is_empty()
    );
    press(&mut app, &transport, KeyCode::Enter).await;
    handle_paste("120", &mut app);
    press(&mut app, &transport, KeyCode::Left).await;
    press(&mut app, &transport, KeyCode::Char('8')).await;
    press(&mut app, &transport, KeyCode::End).await;
    press(&mut app, &transport, KeyCode::Backspace).await;
    handle_paste("000", &mut app);
    press(&mut app, &transport, KeyCode::Esc).await;
    assert_eq!(
        app.auth_dialog.as_ref().unwrap().context_window_size,
        "128000"
    );
    assert_eq!(
        app.auth_dialog.as_ref().unwrap().step,
        AuthDialogStep::AdvancedConfig
    );

    press(&mut app, &transport, KeyCode::Down).await;
    press(&mut app, &transport, KeyCode::Enter).await;
    handle_paste("81920", &mut app);
    press(&mut app, &transport, KeyCode::Left).await;
    press(&mut app, &transport, KeyCode::Delete).await;
    press(&mut app, &transport, KeyCode::Enter).await;
    assert_eq!(app.auth_dialog.as_ref().unwrap().max_tokens, "8192");

    press(&mut app, &transport, KeyCode::Down).await;
    press(&mut app, &transport, KeyCode::Enter).await;
    for character in "X-Client=jk v1?".chars() {
        press(&mut app, &transport, KeyCode::Char(character)).await;
    }
    handle_paste("; X-Route=v1", &mut app);
    press(&mut app, &transport, KeyCode::Up).await;
    assert_eq!(app.auth_dialog.as_ref().unwrap().advanced_selected, 5);
    press(&mut app, &transport, KeyCode::Esc).await;
    let dialog = app.auth_dialog.as_ref().unwrap();
    assert_eq!(dialog.custom_headers, "X-Client=jk v1?; X-Route=v1");
    assert!(dialog.advanced_input.is_none());
    let config = validate_generation_config(dialog).unwrap().unwrap();
    assert_eq!(config.context_window_size, Some(128000));
    assert_eq!(config.max_tokens, Some(8192));
    assert_eq!(
        parse_dialog_custom_headers(&dialog.custom_headers)
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn auth_advanced_invalid_value_blocks_continue_and_can_be_cleared() {
    let transport = RuntimeTransport::in_memory().await.unwrap();
    let mut app = advanced_app();
    app.auth_dialog.as_mut().unwrap().advanced_selected = 3;
    press(&mut app, &transport, KeyCode::Enter).await;
    handle_paste("invalid", &mut app);
    press(&mut app, &transport, KeyCode::Enter).await;
    for _ in 0..3 {
        press(&mut app, &transport, KeyCode::Up).await;
    }
    press(&mut app, &transport, KeyCode::Enter).await;
    let dialog = app.auth_dialog.as_ref().unwrap();
    assert_eq!(dialog.step, AuthDialogStep::AdvancedConfig);
    assert!(dialog.error.is_some());
    for _ in 0..3 {
        press(&mut app, &transport, KeyCode::Down).await;
    }
    press(&mut app, &transport, KeyCode::Enter).await;
    handle_key(
        KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
        &mut app,
        &transport,
    )
    .await
    .unwrap();
    press(&mut app, &transport, KeyCode::Esc).await;
    assert!(
        validate_generation_config(app.auth_dialog.as_ref().unwrap())
            .unwrap()
            .is_none()
    );
}

#[test]
fn auth_advanced_long_input_renders_the_cursor_viewport() {
    let mut app = advanced_app();
    let dialog = app.auth_dialog.as_mut().unwrap();
    dialog.advanced_selected = 5;
    dialog.custom_headers = "X-Client=abcdefghijklmnopqrstuvwxyz".to_owned();
    dialog.start_advanced_edit();
    let area = Rect::new(0, 0, 24, 5);
    let prefix_width = display_width("› ") as u16;
    for start in [false, true] {
        if start {
            app.auth_dialog
                .as_mut()
                .unwrap()
                .advanced_input
                .as_mut()
                .unwrap()
                .move_to_start();
        }
        let input = app
            .auth_dialog
            .as_ref()
            .unwrap()
            .advanced_input
            .as_ref()
            .unwrap();
        let viewport = input.viewport(area.width - prefix_width, 1);
        assert_eq!(
            composer_cursor_position(area, &app),
            Some((prefix_width + viewport.cursor.0, 1))
        );
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        terminal
            .draw(|frame| draw_bottom_pane(frame, area, &app))
            .unwrap();
        let row = (0..area.width)
            .map(|x| terminal.backend().buffer()[(x, 1)].symbol())
            .collect::<String>();
        assert!(row.contains(&viewport.lines[0]), "{row:?} != {viewport:?}");
    }
}
