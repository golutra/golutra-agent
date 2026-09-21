//! 调试快照刷新、完整历史和只读事件检查器的回归验收。

use super::*;
use golutra_agent_core::{
    PostTaskJob, PostTaskJobId, PostTaskJobKind, PostTaskJobStatus, ToolCallId, ToolResultEnvelope,
    ToolResultStatus,
};

fn app() -> TuiApp {
    TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        true,
        "mock".into(),
        None,
    )
}

fn tool() -> ToolResultEnvelope {
    ToolResultEnvelope {
        tool_call_id: ToolCallId::new(),
        tool_name: "shell".into(),
        status: ToolResultStatus::Ok,
        summary: "completed".into(),
        structured_facts: json!({}),
        model_visible_excerpt: None,
        raw_artifact_ref: None,
        evidence_refs: Vec::new(),
        risk: "none".into(),
        verification_hint: None,
    }
}

#[test]
fn debug_refresh_deduplicates_nonadjacent_tools_and_updates_job_status() {
    let mut previous = debug_projection_with_events(SessionId::new(), None, Vec::new());
    previous.tool_results = vec![tool(), tool()];
    previous.post_task_jobs.push(PostTaskJob {
        job_id: PostTaskJobId::new(),
        kind: PostTaskJobKind::DeepEvaluation,
        workspace_id: "test".into(),
        session_id: previous.session_id.to_string(),
        task_id: TaskId::new(),
        input_refs: Vec::new(),
        status: PostTaskJobStatus::Running,
        attempt: 1,
        max_attempts: 3,
        lease_owner: None,
        lease_expires_at: None,
        result_refs: Vec::new(),
        last_error: None,
        created_at: chrono::Utc::now(),
        started_at: None,
        completed_at: None,
    });
    let mut latest = previous.clone();
    latest.post_task_jobs[0].status = PostTaskJobStatus::Succeeded;
    latest.post_task_jobs[0].completed_at = Some(chrono::Utc::now());
    for _ in 0..8 {
        previous = merge_debug_projection(previous, latest.clone());
        assert_eq!(previous.tool_results.len(), 2);
        assert_eq!(previous.post_task_jobs.len(), 1);
        assert_eq!(
            previous.post_task_jobs[0].status,
            PostTaskJobStatus::Succeeded
        );
        assert!(previous.trace_complete);
    }
    latest.session_id = SessionId::new();
    latest.tool_results.clear();
    assert!(
        merge_debug_projection(previous, latest)
            .tool_results
            .is_empty()
    );
}

#[test]
fn debug_refresh_failure_keeps_last_snapshot_and_timestamp() {
    let mut app = app();
    let projection = debug_projection_with_events(app.session_id, None, Vec::new());
    app.apply_developer_projection_result(Ok(projection.clone()));
    let timestamp = app.developer_updated_at;
    app.apply_developer_projection_result(Err("temporary disconnect".into()));
    assert_eq!(app.developer_projection, Some(projection));
    assert_eq!(app.developer_updated_at, timestamp);
    assert!(footer_context_text(&app, 120).contains("update failed"));
}

#[test]
fn debug_history_retains_unarchived_events_above_window_budget() {
    let mut app = app();
    app.enable_inline_history();
    let events = (1..=33_000)
        .map(|sequence| {
            transcript_event(
                sequence,
                app.session_id,
                TaskId::new(),
                RuntimeEventType::CommandAccepted,
                json!({"summary": "retained"}),
            )
        })
        .collect::<Vec<_>>();
    app.replace_event_history(events, false);
    assert_eq!(app.events.len(), 33_000);
    assert!(!app.trim_event_history());
    assert_eq!(app.events.first().unwrap().sequence_no, 1);
    assert!(!app.history_has_more_before);
}

#[test]
fn debug_timeline_reuses_unchanged_layout_and_invalidates_on_resize_and_events() {
    let mut app = app();
    app.append_event_to_history(transcript_event(1, app.session_id, TaskId::new(), RuntimeEventType::ProviderFailed, json!({"error": {"message": "upstream unavailable"}, "error_metadata": {"http_status": 503}})));
    let first = debug_split_live_lines(&app, 120, 20);
    let same = debug_split_live_lines(&app, 120, 20);
    assert!(Arc::ptr_eq(&first, &same));
    assert!(!Arc::ptr_eq(&first, &debug_split_live_lines(&app, 80, 20)));
    app.developer_observations_expanded = !app.developer_observations_expanded;
    assert!(!Arc::ptr_eq(&same, &debug_split_live_lines(&app, 120, 20)));
    app.append_event_to_history(transcript_event(
        2,
        app.session_id,
        TaskId::new(),
        RuntimeEventType::CommandAccepted,
        json!({"summary": "new event"}),
    ));
    assert!(!Arc::ptr_eq(&same, &debug_split_live_lines(&app, 120, 20)));
    let summary = developer_event_summary(&app.events[0]);
    assert!(summary.contains("upstream unavailable"));
    assert!(summary.contains("http_status=503"));
}

#[tokio::test]
async fn debug_event_inspector_preserves_draft_and_returns_to_chat() {
    let mut app = app();
    app.input.set_text("unfinished draft");
    app.append_event_to_history(transcript_event(
        1,
        app.session_id,
        TaskId::new(),
        RuntimeEventType::ProviderFailed,
        json!({"error": "test error"}),
    ));
    let transport = RuntimeTransport::in_memory().await.unwrap();
    handle_key(
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT),
        &mut app,
        &transport,
    )
    .await
    .unwrap();
    assert!(app.developer_detail.is_some());
    handle_paste("must not modify the composer", &mut app);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| draw_ui(frame, &mut app)).unwrap();
    assert!(terminal_buffer_text(&terminal).contains("test error"));
    for (width, height) in [(1, 1), (8, 3)] {
        let mut tiny = Terminal::new(TestBackend::new(width, height)).unwrap();
        tiny.draw(|frame| draw_ui(frame, &mut app)).unwrap();
    }
    handle_key(
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        &mut app,
        &transport,
    )
    .await
    .unwrap();
    assert!(app.developer_detail.is_none());
    assert_eq!(app.input.text(), "unfinished draft");
}

#[test]
fn debug_error_summary_keeps_cause_and_details_use_causal_request_identity() {
    let app = app();
    let mut event = transcript_event(
        1,
        app.session_id,
        TaskId::new(),
        RuntimeEventType::ProviderFailed,
        json!({"summary": "provider failed", "error": {"message": "invalid request"}, "metrics": {"duration_ms": 57000}}),
    );
    let id = golutra_agent_core::ProviderRequestId::new();
    event.causal_context.provider_request_id = Some(id);
    let summary = developer_event_summary(&event);
    assert!(summary.contains("provider failed"));
    assert!(summary.contains("invalid request"));
    assert!(summary.contains("elapsed_ms=57000"));
    assert!(!summary.contains(&id.to_string()));
    assert!(
        developer_view::diagnostic_fields(&event).contains(&("request_id", Some(id.to_string())))
    );
}
