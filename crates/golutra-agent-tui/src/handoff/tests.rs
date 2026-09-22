//! 草稿编辑、取消、会话切换与原生终端选择回归。
use super::*;
use ratatui::backend::Backend;

async fn review() -> (TuiApp, RuntimeTransport) {
    let transport = RuntimeTransport::in_memory().await.unwrap();
    let mut app = TuiApp::new(
        transport.default_thread_id(),
        transport.default_session_id(),
        None,
        false,
        "ready (mock)".into(),
        None,
    );
    transport
        .send_command(session_command(
            app.session_id,
            SessionCommandKind::Create,
            json!({"_thread_id": app.thread_id}),
        ))
        .await
        .unwrap();
    app.input.set_text("original composer draft");
    app.handoff = Some(HandoffFlow {
        source_thread: app.thread_id,
        source_session: app.session_id,
        destination_thread: ThreadId::new(),
        destination_session: SessionId::new(),
        stage: Stage::Review,
        draft: ComposerInput::from_text("Goal: test parser"),
        error: None,
        cancellation: CancellationToken::new(),
        operation: None,
    });
    (app, transport)
}

async fn finish(app: &mut TuiApp) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while app
            .handoff
            .as_ref()
            .is_some_and(|flow| flow.operation.is_some())
        {
            app.poll_handoff().await;
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[test]
fn handoff_slash_parses_optional_multiline_goal_and_is_searchable() {
    assert_eq!(
        parse_slash_input("/handoff"),
        SlashInput::Command(SlashCommand::Handoff { goal: None })
    );
    assert_eq!(
        parse_slash_input("/handoff 修复 parser\n保留 API"),
        SlashInput::Command(SlashCommand::Handoff {
            goal: Some("修复 parser\n保留 API".into())
        })
    );
    assert!(!slash_command_candidates("/hand").is_empty());
}

#[tokio::test]
async fn handoff_editor_preserves_native_selection_cancel_and_unicode_edits() {
    let (mut app, transport) = review().await;
    let source = app.session_id;
    assert_eq!(app.overlay_surface(), Some(OverlaySurface::Handoff));
    assert!(overlay_uses_native_mouse(&app));
    handle_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &mut app,
        &transport,
    );
    paste(&mut app, "测试 🦀\nnext step");
    assert!(
        app.handoff
            .as_ref()
            .unwrap()
            .draft
            .text()
            .contains("parser\n测试 🦀\nnext step")
    );
    handle_key(
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        &mut app,
        &transport,
    );
    assert!(app.handoff.is_none());
    assert_eq!(app.session_id, source);
    assert_eq!(app.input.text(), "original composer draft");
}

#[tokio::test]
async fn handoff_confirm_creates_new_session_with_edited_unsent_draft() {
    let (mut app, transport) = review().await;
    let source = app.thread_id;
    paste(&mut app, "\nTests not run — 用户确认");
    let edited = app.handoff.as_ref().unwrap().draft.text().to_owned();
    handle_key(
        KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
        &mut app,
        &transport,
    );
    assert_eq!(app.thread_id, source);
    finish(&mut app).await;
    assert!(app.handoff.is_none(), "{}", app.status_message);
    assert_ne!(app.thread_id, source);
    assert_eq!(app.input.text(), edited);
    assert_eq!(app.task_id, None);
    app.load_recent_history(&transport).await.unwrap();
    assert!(
        app.events
            .iter()
            .all(|event| event.event_type == RuntimeEventType::SessionCreated)
    );
    let child = transport.resume_thread(app.thread_id).await.unwrap();
    assert_eq!(child.parent_thread_id, Some(source));
    app.input.reset();
    app.restore_handoff_draft();
    assert_eq!(app.input.text(), edited);
    app.input.set_text("my newer draft");
    app.restore_handoff_draft();
    assert_eq!(app.input.text(), "my newer draft");
    app.input.reset();
    let mut submitted = app.events[0].clone();
    submitted.event_type = RuntimeEventType::TurnQueued;
    app.events.push(submitted);
    app.restore_handoff_draft();
    assert!(
        app.input.is_empty(),
        "a previously accepted draft must not be restored"
    );
}

#[tokio::test]
async fn handoff_failure_and_stale_generation_never_switch_sessions() {
    let (mut app, transport) = review().await;
    let source = app.session_id;
    let flow = app.handoff.as_mut().unwrap();
    flow.stage = Stage::Generating;
    flow.operation = Some(tokio::spawn(async {
        Err(golutra_agent_client::ClientError::InvalidSession(
            "upstream failed".into(),
        ))
    }));
    finish(&mut app).await;
    assert_eq!(app.session_id, source);
    assert!(
        app.handoff
            .as_ref()
            .unwrap()
            .error
            .as_deref()
            .unwrap()
            .contains("upstream failed")
    );
    handle_key(
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        &mut app,
        &transport,
    );
    assert_eq!(app.input.text(), "original composer draft");
    app.start_handoff(&transport, None);
    app.session_id = SessionId::new();
    let next = app.session_id;
    app.poll_handoff().await;
    assert!(app.handoff.is_none());
    assert_eq!(app.session_id, next);
}

#[tokio::test]
async fn handoff_editor_renders_wrapped_draft_in_small_and_large_terminals() {
    let (mut app, _) = review().await;
    app.handoff
        .as_mut()
        .unwrap()
        .draft
        .set_text("Goal\n".repeat(50) + "最后 🦀");
    for (width, height) in [(12, 4), (80, 24), (120, 40)] {
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw(frame, frame.area(), &app))
            .unwrap();
        let cursor = terminal.backend_mut().get_cursor_position().unwrap();
        assert!(cursor.x < width && cursor.y < height);
    }
}
