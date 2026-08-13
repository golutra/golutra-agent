use chrono::Utc;
use golutra_core::{
    ApprovalId, ApprovalRequest, EventId, QuestionId, SessionId, TaskId, ThreadId, ToolCallId,
    TurnId, UserQuestionMode, UserQuestionOption, UserQuestionPrompt, UserQuestionRequest,
};
use golutra_protocol::{
    RuntimeEvent, RuntimeEventSource, RuntimeEventType, UserProjection, VisibleStep,
};
use serde_json::json;

use super::*;

fn test_app(task_id: Option<TaskId>, auth_dialog: Option<AuthDialogState>) -> TuiApp {
    TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        task_id,
        false,
        "ready (mock)".to_owned(),
        auth_dialog,
    )
}

fn question_dialog(secret: &str) -> QuestionDialogState {
    let mut dialog = QuestionDialogState::new(UserQuestionRequest {
        question_id: QuestionId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        tool_call_id: ToolCallId::new(),
        questions: vec![UserQuestionPrompt {
            id: "output".to_owned(),
            header: format!("token={secret}"),
            question: format!("Authorization: Bearer {secret}"),
            mode: UserQuestionMode::Single,
            options: vec![UserQuestionOption {
                id: "json".to_owned(),
                label: format!("api_key={secret}"),
                description: Some(format!("token={secret}")),
            }],
        }],
    });
    dialog.focus_free_text(0);
    dialog
        .current_free_text_mut()
        .set_text(format!("Authorization: Bearer {secret}"));
    dialog
}

fn populate_snapshot_secrets(app: &mut TuiApp, secret: &str) {
    app.input
        .set_text(format!("Authorization: Bearer {secret}"));
    app.queue_picker = Some(QueuePickerState {
        items: vec![QueuedPrompt {
            turn_id: TurnId::new(),
            prompt: format!("token={secret}"),
            steer: false,
        }],
        selected: 0,
    });
    app.approval_dialog = Some(ApprovalDialogState::new(ApprovalRequest {
        approval_id: ApprovalId::new(),
        task_id: TaskId::new(),
        turn_id: TurnId::new(),
        tool_call_id: ToolCallId::new(),
        tool_name: format!("token={secret}"),
        resource: format!("Authorization: Bearer {secret}"),
        reason: format!("api_key={secret}"),
    }));
    app.question_dialog = Some(question_dialog(secret));

    let mut history_search = HistorySearchState::default();
    history_search
        .input
        .set_text(format!("Authorization: Bearer {secret}"));
    app.history_search = Some(history_search);
    let mut transcript_search = TranscriptSearchState::default();
    transcript_search.input.set_text(format!("token={secret}"));
    app.transcript.search = Some(transcript_search);

    app.runtime_controls.profile_name = Some(format!("token={secret}"));
    app.runtime_controls.custom_model = Some(format!("Authorization: Bearer {secret}"));
    let mut settings = SettingsDialogState::new(
        &app.runtime_controls,
        &app.provider_choices,
        &app.preferences,
        false,
    );
    settings.editing_model = true;
    settings.model_input.set_text(format!("api_key={secret}"));
    app.settings_dialog = Some(settings);
    app.attachments.push(ComposerAttachment {
        path: std::path::PathBuf::from("attachment.txt"),
        display_path: format!("token={secret}.txt"),
        kind: AttachmentKind::Text,
        bytes: 1,
    });
    app.mention_completion = Some(MentionCompletion {
        replacement: 0..1,
        candidates: vec![MentionCandidate {
            kind: MentionKind::File,
            label: format!("token={secret}"),
            insertion: format!("Authorization: Bearer {secret}"),
            detail: format!("api_key={secret}"),
            source_path: None,
        }],
        selected: 0,
    });
}

fn transient_snapshot_state(app: &TuiApp) -> Vec<String> {
    [
        format!("{:?}", app.input),
        format!("{:?}", app.auth_dialog),
        format!("{:?}", app.resume_picker),
        format!("{:?}", app.queue_picker),
        format!("{:?}", app.approval_dialog),
        format!("{:?}", app.question_dialog),
        format!("{:?}", app.settings_dialog),
        format!("{:?}", app.export_flow),
        format!("{:?}", app.history_search),
        format!("{:?}", app.transcript.search),
        format!("{:?}", app.attachments),
        format!("{:?}", app.mention_completion),
        format!("{:?}", app.runtime_controls),
    ]
    .into_iter()
    .collect()
}

fn terminal_event(sequence_no: u64, task_id: TaskId, turn_id: TurnId) -> RuntimeEvent {
    RuntimeEvent {
        schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
        causal_context: Default::default(),
        causal_links: Vec::new(),
        id: EventId::new(),
        sequence_no,
        session_id: SessionId::new(),
        turn_id: Some(turn_id),
        task_id: Some(task_id),
        parent_event_id: None,
        event_type: RuntimeEventType::TaskCompleted,
        timestamp: Utc::now(),
        source: RuntimeEventSource::Runtime,
        payload: json!({"status": "completed"}),
        payload_ref: None,
        durable: true,
    }
}

#[test]
fn submission_anchor_excludes_previous_and_foreign_terminal_events() {
    let task_id = TaskId::new();
    let turn_id = TurnId::new();
    let anchor = SubmissionAnchor {
        command_id: CommandId::new(),
        after_sequence_no: Some(10),
        task_id: Some(task_id),
        turn_id: Some(turn_id),
    };

    let mut app = test_app(None, None);
    for event in [
        terminal_event(10, task_id, turn_id),
        terminal_event(11, TaskId::new(), turn_id),
    ] {
        app.events.push(event);
    }
    let facts = WaitFacts::from_app(&app);
    assert!(!facts.condition_met(&WaitCondition::TaskTerminal, Some(anchor)));

    app.events.push(terminal_event(12, task_id, TurnId::new()));
    let facts = WaitFacts::from_app(&app);
    assert!(facts.condition_met(&WaitCondition::TaskTerminal, Some(anchor)));
    assert!(!facts.condition_met(&WaitCondition::TurnTerminal, Some(anchor)));

    app.events.push(terminal_event(13, task_id, turn_id));
    assert!(WaitFacts::from_app(&app).condition_met(&WaitCondition::TurnTerminal, Some(anchor)));
}

#[test]
fn submission_anchor_resolves_only_its_own_command() {
    let first_command = CommandId::new();
    let second_command = CommandId::new();
    let first_task = TaskId::new();
    let second_task = TaskId::new();
    let first_turn = TurnId::new();
    let second_turn = TurnId::new();
    let mut first = terminal_event(11, first_task, first_turn);
    first.event_type = RuntimeEventType::TaskCreated;
    first.payload = json!({"command_id": first_command});
    let mut second = terminal_event(12, second_task, second_turn);
    second.event_type = RuntimeEventType::TaskCreated;
    second.payload = json!({"command_id": second_command});
    let anchor = SubmissionAnchor {
        command_id: first_command,
        after_sequence_no: Some(10),
        task_id: None,
        turn_id: None,
    };

    let mut app = test_app(None, None);
    app.events = vec![first, second];
    let resolved = WaitFacts::from_app(&app).resolve_anchor(anchor);
    assert_eq!(resolved.task_id, Some(first_task));
    assert_eq!(resolved.turn_id, Some(first_turn));
}

#[test]
fn submission_scoped_wait_state_does_not_reuse_a_stale_projection_task() {
    let first_task = TaskId::new();
    let first_turn = TurnId::new();
    let second_task = TaskId::new();
    let second_turn = TurnId::new();
    let command_id = CommandId::new();
    let mut created = terminal_event(11, second_task, second_turn);
    created.event_type = RuntimeEventType::TaskCreated;
    created.payload = json!({"command_id": command_id});
    let mut second_terminal = terminal_event(12, second_task, second_turn);
    second_terminal.payload = json!({"status": TaskStatus::Failed});

    let mut app = test_app(None, None);
    app.projection = Some(UserProjection {
        session_id: app.session_id,
        task_id: Some(first_task),
        status: TaskStatus::Completed,
        visible_steps: Vec::new(),
        pending_approval: None,
        final_message: Some("first task".to_owned()),
        residual_risks: Vec::new(),
    });
    app.events = vec![
        terminal_event(9, first_task, first_turn),
        created,
        second_terminal,
    ];
    let anchor = SubmissionAnchor {
        command_id,
        after_sequence_no: Some(10),
        task_id: None,
        turn_id: None,
    };
    let facts = WaitFacts::from_app(&app);
    let resolved_scope = facts.response_scope(&WaitCondition::TaskTerminal, Some(anchor));
    assert!(facts.condition_met(&WaitCondition::Idle, Some(anchor)));
    assert!(matches!(
        resolved_scope,
        WaitResponseScope::Submission {
            status: Some(TaskStatus::Failed),
            ..
        }
    ));

    let (projected_task, projected_turn) =
        task_and_turn_for_scope(&app, WaitResponseScope::Current);
    assert_eq!(projected_task, Some(first_task));
    assert_eq!(projected_turn, Some(first_turn));

    let (task_id, turn_id) = task_and_turn_for_scope(&app, resolved_scope);
    assert_eq!(task_id, Some(second_task));
    assert_eq!(turn_id, Some(second_turn));

    let unresolved = SubmissionAnchor {
        command_id: CommandId::new(),
        after_sequence_no: Some(12),
        task_id: None,
        turn_id: None,
    };
    let unresolved_scope = facts.response_scope(&WaitCondition::TaskStarted, Some(unresolved));
    assert!(!facts.condition_met(&WaitCondition::Idle, Some(unresolved)));
    assert!(matches!(
        unresolved_scope,
        WaitResponseScope::Submission { status: None, .. }
    ));
    assert_eq!(
        task_and_turn_for_scope(&app, unresolved_scope),
        (None, None)
    );
}

#[test]
fn submission_scoped_status_uses_projection_at_a_truncated_history_boundary() {
    let task_id = TaskId::new();
    let turn_id = TurnId::new();
    let command_id = CommandId::new();
    let mut queued = terminal_event(21, task_id, turn_id);
    queued.event_type = RuntimeEventType::TurnQueued;
    queued.payload = json!({"command_id": command_id});

    let mut app = test_app(None, None);
    app.projection = Some(UserProjection {
        session_id: app.session_id,
        task_id: Some(task_id),
        status: TaskStatus::Running,
        visible_steps: Vec::new(),
        pending_approval: None,
        final_message: None,
        residual_risks: Vec::new(),
    });
    app.events = vec![queued];
    let anchor = SubmissionAnchor {
        command_id,
        after_sequence_no: Some(20),
        task_id: None,
        turn_id: None,
    };

    let facts = WaitFacts::from_app(&app);
    assert!(facts.condition_met(&WaitCondition::TaskStarted, Some(anchor)));
    assert!(matches!(
        facts.response_scope(&WaitCondition::TaskStarted, Some(anchor)),
        WaitResponseScope::Submission {
            status: Some(TaskStatus::Running),
            ..
        }
    ));
}

#[test]
fn driver_error_codes_are_stable_and_messages_are_bounded() {
    let error = miette::miette!("frame_expired: frozen frame is unavailable");
    assert_eq!(driver_error_code(&error), "frame_expired");
    assert_eq!(
        driver_error_code(&miette::miette!("untyped error")),
        "driver_error"
    );

    let oversized = "x".repeat(3_000);
    let bounded = bounded_error(&oversized);
    assert!(bounded.ends_with("..."));
    assert_eq!(bounded.chars().count(), 2_003);
}

#[test]
fn driver_rejects_session_switching_slash_commands() {
    for command in [
        "/new",
        "/resume",
        "/resume 019f79f6-c084-7210-a891-a12832a20f14",
        "/fork 019f79f6-c084-7210-a891-a12832a20f14",
    ] {
        let error = ensure_session_binding_is_immutable(command)
            .expect_err("session switch must be rejected");
        assert_eq!(driver_error_code(&error), "session_binding_immutable");
    }
    assert!(ensure_session_binding_is_immutable("/status").is_ok());
    assert!(ensure_session_binding_is_immutable("regular prompt").is_ok());
}

#[test]
fn idle_wait_treats_terminal_states_as_quiescent() {
    assert!(is_quiescent_status(TaskStatus::Idle));
    assert!(is_quiescent_status(TaskStatus::Completed));
    assert!(is_quiescent_status(TaskStatus::Cancelled));
    assert!(!is_quiescent_status(TaskStatus::Running));
    assert!(!is_quiescent_status(TaskStatus::WaitingApproval));
}

#[test]
fn explicit_task_binding_is_read_only_for_prompts() {
    assert!(ensure_task_binding_accepts_no_prompt(None).is_ok());
    let error = ensure_task_binding_accepts_no_prompt(Some(TaskId::new()))
        .expect_err("task-bound prompt must be rejected");
    assert_eq!(driver_error_code(&error), "task_binding_read_only");

    let task_id = Some(TaskId::new());
    assert!(ensure_task_binding_allows_slash(task_id, "/status").is_ok());
    assert!(ensure_task_binding_allows_slash(task_id, "/debug").is_ok());
    for command in ["/abort", "/pause", "/approve", "/compact", "/auth mock"] {
        let error = ensure_task_binding_allows_slash(task_id, command)
            .expect_err("task-bound slash control must be rejected");
        assert_eq!(driver_error_code(&error), "task_binding_read_only");
    }
    assert!(
        ensure_task_binding_accepts_no_control(task_id, "takeover")
            .is_err_and(|error| driver_error_code(&error) == "task_binding_read_only")
    );

    let auth_app = test_app(task_id, Some(AuthDialogState::new()));
    for key in [DriverKey::Enter, DriverKey::Char("1".to_owned())] {
        let error = ensure_driver_binding_allows_key(task_id, &auth_app, &key)
            .expect_err("task-bound auth input must be rejected");
        assert_eq!(driver_error_code(&error), "task_binding_read_only");
    }
    assert!(driver_key_starts_approval_shortcut(&DriverKey::Char(
        "yx".to_owned()
    )));
    assert!(!driver_key_starts_approval_shortcut(&DriverKey::Char(
        "xy".to_owned()
    )));
}

#[test]
fn driver_binding_preflight_follows_the_visible_surface() {
    let task_id = TaskId::new();
    let mut app = test_app(Some(task_id), None);
    app.projection = Some(UserProjection {
        session_id: app.session_id,
        task_id: Some(task_id),
        status: TaskStatus::WaitingApproval,
        visible_steps: Vec::new(),
        pending_approval: Some(ApprovalId::new().to_string()),
        final_message: None,
        residual_risks: Vec::new(),
    });
    app.approval_dialog = Some(ApprovalDialogState::new(ApprovalRequest {
        approval_id: ApprovalId::new(),
        task_id,
        turn_id: TurnId::new(),
        tool_call_id: ToolCallId::new(),
        tool_name: "shell".to_owned(),
        resource: "cargo test".to_owned(),
        reason: "process execution requires approval".to_owned(),
    }));

    for key in [DriverKey::Enter, DriverKey::Char("x1".to_owned())] {
        let error = ensure_driver_binding_allows_key(Some(task_id), &app, &key)
            .expect_err("task-bound approval submission must be rejected");
        assert_eq!(driver_error_code(&error), "task_binding_read_only");
    }
    assert!(ensure_driver_binding_allows_key(Some(task_id), &app, &DriverKey::Up).is_ok());

    app.help_dialog = Some(HelpDialogState::new(HelpTopic::Overview, "approval"));
    assert!(
        ensure_driver_binding_allows_key(Some(task_id), &app, &DriverKey::Char("y".to_owned()))
            .is_ok()
    );
    assert!(ensure_driver_binding_allows_key(Some(task_id), &app, &DriverKey::Escape).is_ok());

    app.help_dialog = None;
    app.approval_dialog = None;
    app.question_dialog = Some(QuestionDialogState::new(UserQuestionRequest {
        question_id: QuestionId::new(),
        task_id,
        turn_id: TurnId::new(),
        tool_call_id: ToolCallId::new(),
        questions: vec![UserQuestionPrompt {
            id: "format".to_owned(),
            header: "Output".to_owned(),
            question: "Choose an output format".to_owned(),
            mode: UserQuestionMode::Single,
            options: vec![
                UserQuestionOption {
                    id: "json".to_owned(),
                    label: "JSON".to_owned(),
                    description: None,
                },
                UserQuestionOption {
                    id: "yaml".to_owned(),
                    label: "YAML".to_owned(),
                    description: None,
                },
            ],
        }],
    }));
    let error = ensure_driver_binding_allows_key(Some(task_id), &app, &DriverKey::Enter)
        .expect_err("task-bound question submission must be rejected");
    assert_eq!(driver_error_code(&error), "task_binding_read_only");
    assert!(
        ensure_driver_binding_allows_key(
            Some(task_id),
            &app,
            &DriverKey::Char("answer draft".to_owned())
        )
        .is_ok()
    );

    app.question_dialog = None;
    app.queue_picker = Some(QueuePickerState {
        items: vec![QueuedPrompt {
            turn_id: TurnId::new(),
            prompt: "queued prompt".to_owned(),
            steer: false,
        }],
        selected: 0,
    });
    for key in [
        DriverKey::Delete,
        DriverKey::Backspace,
        DriverKey::Char("d".to_owned()),
    ] {
        let error = ensure_driver_binding_allows_key(Some(task_id), &app, &key)
            .expect_err("task-bound queue cancellation must be rejected");
        assert_eq!(driver_error_code(&error), "task_binding_read_only");
    }
    assert!(ensure_driver_binding_allows_key(Some(task_id), &app, &DriverKey::Enter).is_ok());
}

#[test]
fn driver_binding_preflight_keeps_search_local_and_sessions_fixed() {
    let task_id = TaskId::new();
    let mut app = test_app(Some(task_id), None);
    app.projection = Some(UserProjection {
        session_id: app.session_id,
        task_id: Some(task_id),
        status: TaskStatus::Running,
        visible_steps: Vec::new(),
        pending_approval: None,
        final_message: None,
        residual_risks: Vec::new(),
    });
    app.input.set_text("/not-a-command");
    app.transcript.search = Some(TranscriptSearchState::default());
    assert!(!driver_enter_reaches_composer(&app));
    assert!(ensure_driver_binding_allows_key(Some(task_id), &app, &DriverKey::Enter).is_ok());
    assert!(ensure_driver_binding_allows_key(Some(task_id), &app, &DriverKey::Escape).is_ok());

    app.transcript.search = None;
    app.composer_mode = ComposerMode::VimInsert;
    assert!(ensure_driver_binding_allows_key(Some(task_id), &app, &DriverKey::Escape).is_ok());
    app.composer_mode = ComposerMode::Standard;
    let error = ensure_driver_binding_allows_key(Some(task_id), &app, &DriverKey::Escape)
        .expect_err("composer escape would abort the active task");
    assert_eq!(driver_error_code(&error), "task_binding_read_only");

    app.resume_picker = Some(ResumePickerState::new(vec![ResumeThreadItem {
        thread_id: ThreadId::new(),
        session_id: SessionId::new(),
        title: "other session".to_owned(),
        preview: String::new(),
    }]));
    let error = ensure_driver_binding_allows_key(None, &app, &DriverKey::Enter)
        .expect_err("Driver session binding must remain immutable");
    assert_eq!(driver_error_code(&error), "session_binding_immutable");
}

#[test]
fn driver_binding_preflight_rejects_mutating_mouse_activations() {
    let task_id = Some(TaskId::new());
    for activation in [
        UiMouseActivation::AuthContinue,
        UiMouseActivation::Approval(ApprovalChoice::Once),
        UiMouseActivation::QuestionSubmit,
    ] {
        let error = ensure_driver_binding_allows_mouse(task_id, activation)
            .expect_err("task-bound mouse control must be rejected");
        assert_eq!(driver_error_code(&error), "task_binding_read_only");
    }

    let error = ensure_driver_binding_allows_mouse(None, UiMouseActivation::ResumeSession)
        .expect_err("mouse input cannot switch a Driver session");
    assert_eq!(driver_error_code(&error), "session_binding_immutable");
}

#[test]
fn slash_validation_rejects_malformed_commands() {
    assert!(ensure_slash_input_is_valid("/status").is_ok());
    let error = ensure_slash_input_is_valid("/does-not-exist")
        .expect_err("unknown slash command must be rejected");
    assert_eq!(driver_error_code(&error), "invalid_slash_command");

    let mut app = test_app(None, None);
    app.input.set_text("/f");
    assert!(pending_slash_completion(&app).is_some_and(|candidate| !candidate.execute_on_select));
    app.input
        .set_text("/fork 019f79f6-c084-7210-a891-a12832a20f14");
    assert!(pending_slash_completion(&app).is_none());

    let modal_app = test_app(None, Some(AuthDialogState::new()));
    assert!(ensure_modal_allows_slash(&modal_app, &SlashCommand::Quit).is_ok());
    for command in [
        SlashCommand::Takeover,
        SlashCommand::Abort,
        SlashCommand::Pause,
        SlashCommand::Continue,
        SlashCommand::Approve,
        SlashCommand::Deny,
    ] {
        assert!(ensure_modal_allows_slash(&modal_app, &command).is_ok());
    }
    let error = ensure_modal_allows_slash(&modal_app, &SlashCommand::Auth(SlashAuthCommand::Setup))
        .expect_err("provider setup must remain modal");
    assert_eq!(driver_error_code(&error), "ui_modal_active");
}

#[test]
fn accumulated_driver_input_is_bounded() {
    let mut app = test_app(None, None);
    app.input.set_text("a");
    let mut question = question_dialog("ordinary");
    question.current_free_text_mut().set_text("bb");
    app.question_dialog = Some(question);
    let mut resume = ResumePickerState::new(Vec::new());
    resume.search.set_text("ccc");
    resume.action_input.set_text("dddd");
    app.resume_picker = Some(resume);
    let mut settings = SettingsDialogState::new(
        &app.runtime_controls,
        &app.provider_choices,
        &app.preferences,
        false,
    );
    settings.model_input.set_text("eeeee");
    app.settings_dialog = Some(settings);
    let mut history = HistorySearchState::default();
    history.input.set_text("ffffff");
    app.history_search = Some(history);
    let mut transcript = TranscriptSearchState::default();
    transcript.input.set_text("ggggggg");
    app.transcript.search = Some(transcript);
    app.prompt_stash = Some("hhhhhhhh".to_owned());

    assert_eq!(driver_input_state_bytes(&app), 36);
    assert!(ensure_driver_input_capacity(&app, MAX_DRIVER_INPUT_BYTES - 36).is_ok());
    let error = ensure_driver_input_capacity(&app, MAX_DRIVER_INPUT_BYTES - 35)
        .expect_err("accumulated input must remain bounded");
    assert_eq!(driver_error_code(&error), "input_too_large");
}

#[test]
fn terminal_evaluation_is_derived_from_runtime_events() {
    let task_id = TaskId::new();
    let turn_id = TurnId::new();
    let mut app = test_app(None, None);
    let first_job = Uuid::now_v7().to_string();
    let second_job = Uuid::now_v7().to_string();
    for (sequence_no, job_id) in [(1, &first_job), (2, &second_job)] {
        let mut event = terminal_event(sequence_no, task_id, turn_id);
        event.event_type = RuntimeEventType::PostTaskJobQueued;
        event.payload = json!({"job": {"job_id": job_id}});
        app.events.push(event);
    }
    let mut completed = terminal_event(3, task_id, turn_id);
    completed.event_type = RuntimeEventType::PostTaskJobCompleted;
    completed.payload = json!({"job_id": first_job});
    app.events.push(completed);
    assert!(!WaitFacts::from_app(&app).evaluation_terminal(task_id));

    let mut failed = terminal_event(4, task_id, turn_id);
    failed.event_type = RuntimeEventType::PostTaskJobFailed;
    failed.payload = json!({"job_id": second_job});
    app.events.push(failed);
    let facts = WaitFacts::from_app(&app);
    assert!(facts.evaluation_terminal(task_id));
    assert!(!facts.evaluation_terminal(TaskId::new()));

    let stage_task_id = TaskId::new();
    let mut stage_failure = terminal_event(5, stage_task_id, turn_id);
    stage_failure.event_type = RuntimeEventType::PostTaskStageFailed;
    stage_failure.payload = json!({"phase": "evaluation_scheduling", "terminal": true});
    app.events.push(stage_failure);
    assert!(WaitFacts::from_app(&app).evaluation_terminal(stage_task_id));
}

#[test]
fn snapshot_state_redacts_transient_secrets() {
    let secret = "sk-driver-command-secret";
    let mut app = test_app(None, Some(AuthDialogState::new()));
    app.projection = Some(UserProjection {
        session_id: app.session_id,
        task_id: None,
        status: TaskStatus::Running,
        visible_steps: vec![VisibleStep {
            label: format!("token={secret}"),
            status: format!("Authorization: Bearer {secret}"),
            summary: format!("api_key={secret}"),
        }],
        pending_approval: Some(format!("token={secret}")),
        final_message: Some(format!("Authorization: Bearer {secret}")),
        residual_risks: vec![format!("api_key={secret}")],
    });
    app.command_messages.push(TranscriptItem {
        role: TranscriptRole::System,
        title: "Provider error".to_owned(),
        body: vec![format!("Authorization: Bearer {secret}")],
    });
    let dialog = app.auth_dialog.as_mut().expect("auth dialog");
    dialog.api_key = secret.to_owned();
    dialog.custom_headers = format!("Authorization=Bearer {secret}");
    let picker = ResumePickerState::new(vec![ResumeThreadItem {
        thread_id: ThreadId::new(),
        session_id: SessionId::new(),
        title: format!("Authorization: Bearer {secret}"),
        preview: format!("token={secret}"),
    }]);
    app.resume_picker = Some(picker.clone());
    app.export_flow = Some(ExportFlowState {
        picker,
        step: ExportFlowStep::Destination,
        range_input: ComposerInput::from_text("1"),
        destination_input: ComposerInput::from_text(format!("/tmp/token={secret}")),
        error: Some(format!("Authorization: Bearer {secret}")),
        receipt: None,
    });
    populate_snapshot_secrets(&mut app, secret);

    redact_snapshot_ui_state(&mut app);

    let rendered = format!("{app:?}");
    assert!(!rendered.contains(secret));
    assert!(rendered.contains("redacted-secret"));
    assert!(rendered.contains("redacted-provider-headers"));
}

#[tokio::test]
async fn snapshot_render_restores_every_transient_input_surface() {
    let workspace = tempfile::tempdir().expect("workspace");
    let transport = RuntimeTransport::ephemeral_for_cwd(workspace.path())
        .await
        .expect("transport");
    let mut driver = TuiDriver::launch(
        transport,
        DriverLaunchOptions {
            session: None,
            task_id: None,
            debug: true,
            yolo: false,
            execution_mode: TuiExecutionModeArg::Open,
            tool_profile: TuiToolProfileArg::Coding,
            width: 100,
            height: 24,
        },
    )
    .await
    .expect("driver");
    populate_snapshot_secrets(&mut driver.app, "sk-snapshot-state-secret");
    let expected = transient_snapshot_state(&driver.app);

    driver
        .render_frame(&SnapshotRequest {
            scope: SnapshotScope::Screen,
            panes: SnapshotPanes::FullScreen,
            width: 100,
            height: 24,
            rows: None,
            frame_id: None,
            detail: SnapshotDetail::Text,
        })
        .expect("full-screen snapshot");

    assert_eq!(transient_snapshot_state(&driver.app), expected);
}

#[tokio::test]
async fn rejected_mouse_activation_does_not_change_local_selection() {
    let workspace = tempfile::tempdir().expect("workspace");
    let transport = RuntimeTransport::ephemeral_for_cwd(workspace.path())
        .await
        .expect("transport");
    let mut driver = TuiDriver::launch(
        transport,
        DriverLaunchOptions {
            session: None,
            task_id: None,
            debug: false,
            yolo: false,
            execution_mode: TuiExecutionModeArg::Open,
            tool_profile: TuiToolProfileArg::Coding,
            width: 100,
            height: 24,
        },
    )
    .await
    .expect("driver");
    driver.app.auth_dialog = None;
    driver.app.resume_picker = Some(ResumePickerState::new(vec![
        ResumeThreadItem {
            thread_id: ThreadId::new(),
            session_id: SessionId::new(),
            title: "first".to_owned(),
            preview: String::new(),
        },
        ResumeThreadItem {
            thread_id: ThreadId::new(),
            session_id: SessionId::new(),
            title: "second".to_owned(),
            preview: String::new(),
        },
    ]));
    driver.refresh_active_layout().expect("resume layout");
    let region = overlay_mouse_regions(driver.app.layout.transcript, &driver.app)
        .into_iter()
        .find(|region| region.press == UiMousePress::Resume(1))
        .expect("second resume row");

    let response = driver
        .handle(DriverRequest::InputMouse {
            event: golutra_protocol::DriverMouseEvent {
                kind: golutra_protocol::DriverMouseKind::LeftClick,
                column: region.area.x,
                row: region.area.y,
            },
        })
        .await;

    assert!(matches!(
        response,
        DriverResponse::Error { ref code, .. } if code == "session_binding_immutable"
    ));
    assert_eq!(
        driver
            .app
            .resume_picker
            .as_ref()
            .expect("resume picker")
            .selected,
        0
    );
    assert_eq!(driver.app.mouse_press, None);
}

#[test]
fn frame_cache_enforces_capacity_and_ttl() {
    fn frame(id: usize) -> TuiFrame {
        TuiFrame {
            frame_id: format!("frame-{id}"),
            instance_id: "instance".to_owned(),
            workspace_id: "workspace".to_owned(),
            session_id: "session".to_owned(),
            task_id: None,
            turn_id: None,
            event_high_watermark: None,
            width: 80,
            height: 20,
            scope: SnapshotScope::Session,
            panes: SnapshotPanes::Transcript,
            total_rows: 0,
            returned_range: RowRange { start: 0, end: 0 },
            lines: Vec::new(),
            complete: true,
            missing_sections: Vec::new(),
            redaction_status: RedactionStatus::NotRequired,
            next_range: None,
            hit_regions: Vec::new(),
            cells: None,
        }
    }

    let now = tokio::time::Instant::now();
    let expired_at = now
        .checked_sub(FRAME_CACHE_TTL + Duration::from_millis(1))
        .expect("expired instant");
    let mut cache = VecDeque::from([CachedFrame {
        created_at: expired_at,
        frame: frame(0),
    }]);
    prune_expired_frames(&mut cache, now);
    assert!(cache.is_empty());

    for id in 0..=FRAME_CACHE_CAPACITY {
        cache_frame(&mut cache, frame(id), now);
    }
    assert_eq!(cache.len(), FRAME_CACHE_CAPACITY);
    assert_eq!(
        cache.front().map(|cached| cached.frame.frame_id.as_str()),
        Some("frame-1")
    );
    assert_eq!(
        cache.back().map(|cached| cached.frame.frame_id.as_str()),
        Some("frame-8")
    );
}

#[test]
fn rejected_runtime_ack_is_not_reported_as_accepted() {
    let rejected = CommandAck {
        command_id: CommandId::new(),
        accepted: false,
        reason: Some("observer cannot abort".to_owned()),
    };
    let error = ensure_command_accepted(&rejected, "fallback")
        .expect_err("rejected ack must remain rejected");
    assert_eq!(driver_error_code(&error), "command_rejected");
    assert!(error.to_string().contains("observer cannot abort"));

    let accepted = CommandAck {
        command_id: CommandId::new(),
        accepted: true,
        reason: None,
    };
    assert!(ensure_command_accepted(&accepted, "fallback").is_ok());
}

#[tokio::test]
async fn snapshot_render_does_not_mutate_the_active_pane_layout() {
    let workspace = tempfile::tempdir().expect("workspace");
    let transport = RuntimeTransport::ephemeral_for_cwd(workspace.path())
        .await
        .expect("transport");
    let mut driver = TuiDriver::launch(
        transport,
        DriverLaunchOptions {
            session: None,
            task_id: None,
            debug: true,
            yolo: false,
            execution_mode: TuiExecutionModeArg::Open,
            tool_profile: TuiToolProfileArg::Coding,
            width: 100,
            height: 24,
        },
    )
    .await
    .expect("driver");
    driver.app.auth_dialog = None;
    let session_id = driver.app.session_id;
    driver.app.developer_projection = Some(golutra_protocol::DebugProjection {
        session_id,
        task_id: None,
        event_window: golutra_protocol::DebugEventWindow {
            start_cursor: Some(1),
            end_cursor: Some(30),
            has_more_before: false,
            limit: 256,
        },
        events: (1..=30)
            .map(|sequence_no| RuntimeEvent {
                schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
                causal_context: Default::default(),
                causal_links: Vec::new(),
                id: EventId::new(),
                sequence_no,
                session_id,
                turn_id: None,
                task_id: None,
                parent_event_id: None,
                event_type: RuntimeEventType::StepCompleted,
                timestamp: Utc::now(),
                source: RuntimeEventSource::Runtime,
                payload: json!({"summary": "wrapped developer detail ".repeat(12)}),
                payload_ref: None,
                durable: true,
            })
            .collect(),
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
    driver.refresh_active_layout().expect("active layout");
    driver.app.transcript.top_row_override = Some(3);

    let expected_layout = driver.app.layout;
    let expected_transcript_top_row = driver.app.transcript.top_row_override;

    driver
        .render_frame(&SnapshotRequest {
            scope: SnapshotScope::Screen,
            panes: SnapshotPanes::Transcript,
            width: 100,
            height: 24,
            rows: None,
            frame_id: None,
            detail: SnapshotDetail::Text,
        })
        .expect("transcript snapshot");

    assert_eq!(driver.app.layout, expected_layout);
    assert_eq!(
        driver.app.transcript.top_row_override,
        expected_transcript_top_row
    );
}
