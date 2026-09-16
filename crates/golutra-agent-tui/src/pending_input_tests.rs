use super::*;

fn pending_app() -> TuiApp {
    TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready".into(),
        None,
    )
}

fn event(
    app: &TuiApp,
    sequence: u64,
    turn: TurnId,
    kind: RuntimeEventType,
    payload: Value,
) -> RuntimeEvent {
    let mut event = transcript_event(sequence, app.session_id, TaskId::new(), kind, payload);
    event.turn_id = Some(turn);
    event
}

fn messages(app: &TuiApp) -> Vec<(TranscriptRole, String)> {
    transcript_view::event_operation_entries(&app.events)
        .into_iter()
        .map(|entry| {
            let item = entry.projection.item(false);
            (item.role.clone(), item.body.join("\n"))
        })
        .collect()
}

#[test]
fn pending_input_commits_at_start_after_previous_reply_and_only_once() {
    let mut app = pending_app();
    let first = TurnId::new();
    let next = TurnId::new();
    app.events = vec![
        event(
            &app,
            1,
            first,
            RuntimeEventType::TaskCreated,
            json!({"payload":{"prompt":"你好"}}),
        ),
        event(
            &app,
            2,
            next,
            RuntimeEventType::TurnQueued,
            json!({"payload":{"prompt":"hi"}}),
        ),
        event(
            &app,
            3,
            first,
            RuntimeEventType::AssistantMessage,
            json!({"content":"你好。"}),
        ),
    ];
    assert_eq!(
        messages(&app),
        vec![
            (TranscriptRole::User, "你好".into()),
            (TranscriptRole::Assistant, "你好。".into())
        ]
    );
    assert_eq!(queued_prompts(&app.events)[0].prompt, "hi");
    let start = event(
        &app,
        4,
        next,
        RuntimeEventType::TurnStarted,
        json!({"prompt":"hi"}),
    );
    app.events.push(start.clone());
    app.events.push(start);
    app.events.push(event(
        &app,
        5,
        next,
        RuntimeEventType::AssistantMessage,
        json!({"content":"Hi."}),
    ));
    assert!(queued_prompts(&app.events).is_empty());
    let expected = vec![
        (TranscriptRole::User, "你好".into()),
        (TranscriptRole::Assistant, "你好。".into()),
        (TranscriptRole::User, "hi".into()),
        (TranscriptRole::Assistant, "Hi.".into()),
    ];
    assert_eq!(messages(&app), expected);
    app.events.reverse();
    assert_eq!(messages(&app), expected);
}

#[test]
fn pending_input_edit_and_cancel_never_mutate_committed_history() {
    let mut app = pending_app();
    let edited = TurnId::new();
    let cancelled = TurnId::new();
    app.events = vec![
        event(
            &app,
            1,
            edited,
            RuntimeEventType::TurnQueued,
            json!({"payload":{"prompt":"old"}}),
        ),
        event(
            &app,
            2,
            cancelled,
            RuntimeEventType::TurnQueued,
            json!({"payload":{"prompt":"cancel me"}}),
        ),
        event(
            &app,
            3,
            edited,
            RuntimeEventType::TurnUpdated,
            json!({"prompt":"edited"}),
        ),
        event(
            &app,
            4,
            cancelled,
            RuntimeEventType::TurnCancelled,
            json!({}),
        ),
    ];
    assert!(messages(&app).is_empty());
    assert_eq!(
        queued_prompts(&app.events)
            .iter()
            .map(|p| p.prompt.as_str())
            .collect::<Vec<_>>(),
        vec!["edited"]
    );
    app.events.push(event(
        &app,
        5,
        edited,
        RuntimeEventType::TurnStarted,
        json!({"prompt":"wire text"}),
    ));
    assert_eq!(
        messages(&app),
        vec![(TranscriptRole::User, "edited".into())]
    );
    app.events.push(event(
        &app,
        6,
        edited,
        RuntimeEventType::TurnCancelled,
        json!({}),
    ));
    assert_eq!(
        messages(&app),
        vec![(TranscriptRole::User, "edited".into())]
    );
}

#[test]
fn pending_input_start_without_queue_page_still_displays_prompt() {
    let mut app = pending_app();
    let turn = TurnId::new();
    app.events.push(event(
        &app,
        20,
        turn,
        RuntimeEventType::TurnStarted,
        json!({"prompt":"recovered"}),
    ));
    assert_eq!(
        messages(&app),
        vec![(TranscriptRole::User, "recovered".into())]
    );
}

#[tokio::test]
async fn interrupted_queue_restores_only_unconsumed_inputs_and_attachments() {
    let transport = RuntimeTransport::in_memory().await.unwrap();
    let mut app = pending_app();
    let consumed = TurnId::new();
    let waiting = TurnId::new();
    app.events = vec![
        event(
            &app,
            1,
            consumed,
            RuntimeEventType::TurnQueued,
            json!({"prompt":"already handled", "steer":true}),
        ),
        event(
            &app,
            2,
            waiting,
            RuntimeEventType::TurnQueued,
            json!({"prompt":"pending text", "attachments":[{"path":"diagram.png","kind":"image","bytes":12}]}),
        ),
    ];
    app.input.set_text("unsent draft");
    app.capture_pending_recovery(false);
    app.apply_runtime_event(event(
        &app,
        3,
        consumed,
        RuntimeEventType::TurnStarted,
        json!({"prompt":"already handled"}),
    ));
    app.apply_runtime_event(event(
        &app,
        4,
        waiting,
        RuntimeEventType::TaskAborted,
        json!({}),
    ));
    assert!(app.poll_pending_recovery(&transport).await.unwrap());
    assert_eq!(app.input.text(), "pending text\nunsent draft");
    assert_eq!(app.attachments[0].display_path, "diagram.png");
    assert_eq!(app.attachments[0].kind, AttachmentKind::Image);
    assert!(!app.poll_pending_recovery(&transport).await.unwrap());
    assert_eq!(app.input.text(), "pending text\nunsent draft");
}

#[tokio::test]
async fn interruption_captures_late_queue_receipts_and_keeps_edited_steer_mode() {
    let transport = RuntimeTransport::in_memory().await.unwrap();
    let mut app = pending_app();
    let turn = TurnId::new();
    app.capture_pending_recovery(false);
    app.apply_runtime_event(event(
        &app,
        1,
        turn,
        RuntimeEventType::TurnQueued,
        json!({"prompt":"late", "steer":true}),
    ));
    app.apply_runtime_event(event(
        &app,
        2,
        turn,
        RuntimeEventType::TurnUpdated,
        json!({"prompt":"edited late"}),
    ));
    assert!(queued_prompts(&app.events)[0].steer);
    app.apply_runtime_event(event(
        &app,
        3,
        turn,
        RuntimeEventType::TaskAborted,
        json!({}),
    ));
    assert!(app.poll_pending_recovery(&transport).await.unwrap());
    assert_eq!(app.input.text(), "edited late");
}

#[tokio::test]
async fn explicitly_rejected_steers_wait_together_without_starting_a_task() {
    let transport = RuntimeTransport::in_memory().await.unwrap();
    let mut app = TuiApp::new(
        transport.default_thread_id(),
        transport.default_session_id(),
        None,
        false,
        "ready".into(),
        None,
    );
    for prompt in ["correction one", "correction two"] {
        app.projection = Some(UserProjection {
            session_id: app.session_id,
            task_id: Some(TaskId::new()),
            status: golutra_agent_core::TaskStatus::Running,
            visible_steps: Vec::new(),
            pending_approval: None,
            final_message: None,
            residual_risks: Vec::new(),
        });
        app.input.set_text(prompt);
        app.send_enter_prompt(&transport, prompt.into())
            .await
            .unwrap();
        assert!(app.input.is_empty());
        assert!(!app.last_prompt_ack.as_ref().unwrap().accepted);
    }
    assert_eq!(
        app.rejected_steer_previews()
            .iter()
            .map(|input| input.prompt.as_str())
            .collect::<Vec<_>>(),
        vec!["correction one", "correction two"]
    );
    assert!(!has_active_task(&app));
    assert!(
        app.events
            .iter()
            .all(|event| event.event_type != RuntimeEventType::TaskCreated)
    );
}

#[tokio::test]
async fn recovered_history_reconciles_consumed_input_before_restoring_the_queue() {
    let transport = RuntimeTransport::in_memory().await.unwrap();
    let mut app = pending_app();
    let consumed = TurnId::new();
    let waiting = TurnId::new();
    app.events = vec![
        event(
            &app,
            1,
            consumed,
            RuntimeEventType::TurnQueued,
            json!({"prompt":"consumed"}),
        ),
        event(
            &app,
            2,
            waiting,
            RuntimeEventType::TurnQueued,
            json!({"prompt":"still pending"}),
        ),
    ];
    app.capture_pending_recovery(false);
    app.events.push(event(
        &app,
        3,
        consumed,
        RuntimeEventType::TurnStarted,
        json!({"prompt":"consumed"}),
    ));
    app.events.push(event(
        &app,
        4,
        consumed,
        RuntimeEventType::TaskInterrupted,
        json!({}),
    ));
    let original_session = app.session_id;
    app.session_id = SessionId::new();
    assert!(!app.poll_pending_recovery(&transport).await.unwrap());
    assert!(app.input.is_empty());
    app.session_id = original_session;
    assert!(app.poll_pending_recovery(&transport).await.unwrap());
    assert_eq!(app.input.text(), "still pending");
}

#[test]
fn pending_input_start_during_edit_keeps_unsent_draft() {
    let mut app = pending_app();
    let turn = TurnId::new();
    app.editing_queued_turn = Some(turn);
    app.input.set_text("my unsent correction");
    app.apply_runtime_event(event(
        &app,
        1,
        turn,
        RuntimeEventType::TurnStarted,
        json!({"prompt":"original"}),
    ));
    assert!(app.editing_queued_turn.is_none());
    assert_eq!(app.input.text(), "my unsent correction");
    assert_eq!(
        messages(&app),
        vec![(TranscriptRole::User, "original".into())]
    );
}

#[test]
fn pending_input_never_archives_preview_and_commits_user_before_reply() {
    let mut app = pending_app();
    let first = TurnId::new();
    let next = TurnId::new();
    app.events = vec![
        event(
            &app,
            1,
            first,
            RuntimeEventType::TaskCreated,
            json!({"prompt":"first prompt"}),
        ),
        event(
            &app,
            2,
            next,
            RuntimeEventType::TurnQueued,
            json!({"prompt":"next prompt"}),
        ),
        event(
            &app,
            3,
            first,
            RuntimeEventType::AssistantMessage,
            json!({"content":"first reply"}),
        ),
    ];
    app.enable_inline_history();
    let mut terminal = Terminal::with_options(
        TestBackend::new(80, 40),
        TerminalOptions {
            viewport: Viewport::Inline(4),
        },
    )
    .expect("inline terminal");
    let mut history = InlineHistoryState::new(app.session_id);
    history.flush(&mut terminal, &mut app).expect("first flush");
    assert!(
        !app.transcript
            .history
            .committed_event_ids
            .contains(&app.events[1].id)
    );
    let archived = app.transcript.history.committed_event_ids.clone();
    let started = event(
        &app,
        4,
        next,
        RuntimeEventType::TurnStarted,
        json!({"prompt":"next prompt"}),
    );
    let start_id = started.id;
    app.apply_runtime_event(started);
    app.apply_runtime_event(event(
        &app,
        5,
        next,
        RuntimeEventType::AssistantMessage,
        json!({"content":"next reply"}),
    ));
    history
        .flush(&mut terminal, &mut app)
        .expect("second flush");
    assert!(archived.is_subset(&app.transcript.history.committed_event_ids));
    assert!(
        app.transcript
            .history
            .committed_event_ids
            .contains(&start_id)
    );
    let entries = transcript_view::history_event_operations(&app);
    let body = entries
        .iter()
        .map(|entry| entry.projection.item(false).body.join("\n"))
        .collect::<Vec<_>>();
    assert_eq!(
        body,
        vec!["first prompt", "first reply", "next prompt", "next reply"]
    );
    assert!(pending_input::preview_lines(&app, 80).is_empty());
}

#[test]
fn pending_input_preview_sits_above_composer_and_cursor_at_all_sizes() {
    let mut app = pending_app();
    app.input.set_text("draft");
    app.events = vec![
        event(
            &app,
            1,
            TurnId::new(),
            RuntimeEventType::TurnQueued,
            json!({"payload":{"prompt":"next question"}}),
        ),
        event(
            &app,
            2,
            TurnId::new(),
            RuntimeEventType::TurnQueued,
            json!({"payload":{"prompt":"clarify now", "steer":true}}),
        ),
    ];
    for (width, height) in [(80, 20), (40, 14), (20, 8), (8, 5)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| draw_ui(frame, &mut app))
            .expect("draw");
        let text = terminal_buffer_text(&terminal);
        let (x, y) = composer_cursor_position(app.layout.bottom, &app).expect("cursor");
        assert!(x < width && y < height, "{width}x{height}: {x},{y}");
        let row = (0..width)
            .map(|x| terminal.backend().buffer()[(x, y)].symbol())
            .collect::<String>();
        assert!(row.contains("draft"), "{width}x{height}: {text}");
        if width == 80 {
            assert!(text.find("clarify now").unwrap() < text.find("next question").unwrap());
            assert!(text.find("next question").unwrap() < text.find("› draft").unwrap());
            assert_eq!(text.matches("next question").count(), 1);
        }
    }
}

#[test]
fn pending_input_long_preview_is_bounded_without_changing_message() {
    let mut app = pending_app();
    let prompt = "中文长消息\n".repeat(1000);
    app.events.push(event(
        &app,
        1,
        TurnId::new(),
        RuntimeEventType::TurnQueued,
        json!({"prompt":prompt}),
    ));
    let lines = pending_input::preview_lines(&app, 30);
    assert!(lines.len() <= 7);
    assert!(lines.iter().any(|line| line.to_string().contains('…')));
    assert_eq!(queued_prompts(&app.events)[0].prompt, prompt);
}

#[tokio::test]
async fn pending_input_alt_up_edits_latest_and_preserves_nonempty_draft() {
    let transport = RuntimeTransport::in_memory().await.expect("transport");
    let mut app = pending_app();
    let latest = TurnId::new();
    app.events = vec![
        event(
            &app,
            1,
            TurnId::new(),
            RuntimeEventType::TurnQueued,
            json!({"prompt":"first"}),
        ),
        event(
            &app,
            2,
            latest,
            RuntimeEventType::TurnQueued,
            json!({"prompt":"latest"}),
        ),
    ];
    app.input.set_text("keep draft");
    handle_key(
        KeyEvent::new(KeyCode::Up, KeyModifiers::ALT),
        &mut app,
        &transport,
    )
    .await
    .expect("key");
    assert_eq!(app.input.text(), "keep draft");
    assert!(app.editing_queued_turn.is_none());
    app.input.clear();
    handle_key(
        KeyEvent::new(KeyCode::Up, KeyModifiers::ALT),
        &mut app,
        &transport,
    )
    .await
    .expect("key");
    assert_eq!(app.input.text(), "latest");
    assert_eq!(app.editing_queued_turn, Some(latest));
    handle_key(
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .expect("escape");
    assert!(app.editing_queued_turn.is_none());
    assert_eq!(queued_prompts(&app.events).len(), 2);
}
