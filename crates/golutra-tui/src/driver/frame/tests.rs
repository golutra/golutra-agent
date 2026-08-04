use chrono::Utc;
use golutra_core::{
    BudgetState, EventId, LoopAction, LoopDecision, LoopDecisionId, RedactionStatus, SessionId,
    ThreadId, ToolCallId, ToolResultEnvelope, ToolResultStatus,
};
use golutra_protocol::{DebugEventWindow, RuntimeEventSource, RuntimeEventType};
use ratatui::style::Style;
use serde_json::json;

use super::*;
use crate::BodyLayoutMode;

fn event(
    sequence_no: u64,
    session_id: SessionId,
    task_id: TaskId,
    turn_id: TurnId,
    event_type: RuntimeEventType,
    payload: Value,
) -> RuntimeEvent {
    RuntimeEvent {
        schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
        causal_context: Default::default(),
        causal_links: Vec::new(),
        id: EventId::new(),
        sequence_no,
        session_id,
        turn_id: Some(turn_id),
        task_id: Some(task_id),
        parent_event_id: None,
        event_type,
        timestamp: Utc::now(),
        source: RuntimeEventSource::Runtime,
        payload,
        payload_ref: None,
        durable: true,
    }
}

#[test]
fn current_turn_events_are_scoped_and_redacted() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let old_turn = TurnId::new();
    let current_turn = TurnId::new();
    let values = vec![
        event(
            1,
            session_id,
            task_id,
            old_turn,
            RuntimeEventType::AssistantMessage,
            json!({"summary": "old"}),
        ),
        event(
            2,
            session_id,
            task_id,
            current_turn,
            RuntimeEventType::AssistantMessage,
            json!({
                "api_key": "must-not-leak",
                "summary": "Authorization: Bearer sk-secret-value"
            }),
        ),
    ];

    let scoped = scoped_runtime_events(&values, SnapshotScope::CurrentTurn);
    assert_eq!(scoped.len(), 1);
    let encoded = serde_json::to_string(&scoped).expect("scoped JSON");
    assert!(!encoded.contains("must-not-leak"));
    assert!(!encoded.contains("sk-secret-value"));
    assert!(encoded.contains("redacted-secret"));
}

#[test]
fn composer_text_uses_canonical_redaction() {
    assert_eq!(redacted_ui_text("ordinary draft"), "ordinary draft");
    let redacted = redacted_ui_text("Authorization: Bearer sk-unsent-secret-value");
    assert!(!redacted.contains("sk-unsent-secret-value"));
    assert!(redacted.contains("redacted-secret"));
}

#[test]
fn current_turn_drops_old_tool_facts_and_redacts_loop_reason() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let old_turn = TurnId::new();
    let current_turn = TurnId::new();
    let tool_call_id = ToolCallId::new();
    let projection = DebugProjection {
        session_id,
        task_id: Some(task_id),
        events: vec![
            event(
                1,
                session_id,
                task_id,
                old_turn,
                RuntimeEventType::ToolCompleted,
                json!({"envelope": {"tool_call_id": tool_call_id}}),
            ),
            event(
                2,
                session_id,
                task_id,
                current_turn,
                RuntimeEventType::AssistantMessage,
                json!({"summary": "current response"}),
            ),
        ],
        event_window: DebugEventWindow {
            start_cursor: Some(1),
            end_cursor: Some(2),
            has_more_before: false,
            limit: 256,
        },
        busy_policy_decisions: Vec::new(),
        tool_results: vec![ToolResultEnvelope {
            tool_call_id,
            tool_name: "shell".to_owned(),
            status: ToolResultStatus::Ok,
            summary: "old tool".to_owned(),
            structured_facts: json!({}),
            model_visible_excerpt: None,
            raw_artifact_ref: None,
            evidence_refs: Vec::new(),
            risk: "none".to_owned(),
            verification_hint: None,
        }],
        artifacts: Vec::new(),
        evidence: Vec::new(),
        verification: None,
        loop_decisions: vec![LoopDecision {
            decision_id: LoopDecisionId::new(),
            task_id,
            turn_id: current_turn,
            action: LoopAction::StopSuccess,
            reason: "token=sk-secret-loop-value".to_owned(),
            evidence_refs: Vec::new(),
            verification_ref: None,
            policy_ref: None,
            budget_state: BudgetState {
                planned_input_tokens: None,
                actual_input_tokens: None,
                output_tokens: None,
                total_tokens: None,
                estimated_cost: None,
                budget_remaining: None,
                compact_recommended: false,
                cost_risk: "none".to_owned(),
            },
            tool_state: "none".to_owned(),
            model_state: "complete".to_owned(),
            next_step: None,
        }],
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

    let scoped =
        scoped_debug_projection(projection, SnapshotScope::CurrentTurn).expect("scoped projection");
    assert!(scoped.tool_results.is_empty());
    assert_eq!(scoped.events.len(), 1);
    let encoded = serde_json::to_string(&scoped).expect("projection JSON");
    assert!(!encoded.contains("sk-secret-loop-value"));
    assert!(encoded.contains("redacted-secret"));
}

#[test]
fn cjk_lines_and_cells_skip_continuation_columns() {
    let area = Rect::new(0, 0, 6, 1);
    let mut buffer = Buffer::empty(area);
    buffer.set_string(0, 0, "你A", Style::default());

    let lines = frame_lines(&buffer, area, SnapshotPanes::Transcript);
    assert_eq!(lines[0].text, "你A");
    assert_eq!(lines[0].display_width, 3);

    let cells = frame_cells(&buffer, area, SnapshotPanes::Transcript);
    assert!(
        cells
            .iter()
            .any(|cell| cell.column == 1 && cell.symbol == "你")
    );
    assert!(
        cells
            .iter()
            .any(|cell| cell.column == 3 && cell.symbol == "A")
    );
    assert!(!cells.iter().any(|cell| cell.column == 2));
}

#[test]
fn hit_regions_expose_visible_transcript_operation_toggles() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let turn_id = TurnId::new();
    let tool_call_id = ToolCallId::new();
    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        Some(task_id),
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.events.push(event(
        1,
        session_id,
        task_id,
        turn_id,
        RuntimeEventType::ToolCompleted,
        json!({
            "envelope": {
                "tool_call_id": tool_call_id,
                "tool_name": "shell",
                "status": "ok",
                "summary": "shell command completed",
                "structured_facts": {"command": "cargo test"},
                "model_visible_excerpt": "test result: ok"
            }
        }),
    ));
    let layout = UiLayoutSnapshot {
        body: Rect::new(0, 0, 80, 12),
        transcript: Rect::new(0, 0, 80, 12),
        developer: None,
        bottom: Rect::new(0, 12, 80, 4),
        body_mode: BodyLayoutMode::Transcript,
    };

    let regions = frame_hit_regions(layout, layout.transcript, &app);
    let toggle = regions
        .iter()
        .find(|region| region.id == format!("transcript_operation_toggle:{tool_call_id}"))
        .expect("operation toggle hit region");

    assert_eq!(toggle.pane, TuiHitPane::Transcript);
    assert_eq!(
        (toggle.x, toggle.y, toggle.width, toggle.height),
        (0, 0, 4, 1)
    );
}

#[test]
fn hit_regions_expose_overlay_tabs_and_actions() {
    let mut app = TuiApp::new(
        ThreadId::new(),
        SessionId::new(),
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.help_dialog = Some(crate::HelpDialogState::new(
        crate::HelpTopic::Overview,
        "composer",
    ));
    let layout = UiLayoutSnapshot {
        body: Rect::new(0, 0, 100, 16),
        transcript: Rect::new(0, 0, 100, 16),
        developer: None,
        bottom: Rect::new(0, 16, 100, 4),
        body_mode: BodyLayoutMode::Transcript,
    };

    let regions = frame_hit_regions(layout, Rect::new(0, 0, 100, 21), &app);
    let overlay = regions
        .iter()
        .find(|region| region.id == "overlay")
        .expect("overlay hit region");
    assert_eq!(overlay.pane, TuiHitPane::Overlay);
    assert_eq!(
        (overlay.x, overlay.y, overlay.width, overlay.height),
        (0, 0, 100, 16)
    );
    let whats_new = regions
        .iter()
        .find(|region| region.id == "help_what_s_new")
        .expect("what's new hit region");
    assert_eq!(whats_new.pane, TuiHitPane::Overlay);
    assert_eq!(whats_new.height, 1);
}

#[test]
fn frozen_pages_keep_the_same_digest_and_next_range() {
    let mut frame = TuiFrame {
        frame_id: String::new(),
        instance_id: "instance".to_owned(),
        workspace_id: "workspace".to_owned(),
        session_id: "session".to_owned(),
        task_id: None,
        turn_id: None,
        event_high_watermark: Some(9),
        width: 80,
        height: 20,
        scope: SnapshotScope::Session,
        panes: SnapshotPanes::Transcript,
        total_rows: 4,
        returned_range: RowRange { start: 1, end: 4 },
        lines: (1..=4)
            .map(|row| TuiFrameLine {
                row,
                text: format!("line {row}"),
                display_width: 6,
                pane: TuiFramePane::Transcript,
            })
            .collect(),
        complete: true,
        missing_sections: Vec::new(),
        redaction_status: RedactionStatus::NotRequired,
        next_range: None,
        hit_regions: Vec::new(),
        cells: None,
    };
    frame.frame_id = frame_digest(&frame).expect("frame digest");

    let first =
        slice_frame(frame.clone(), Some(RowRange { start: 1, end: 2 })).expect("first page");
    let second = slice_frame(frame, first.next_range).expect("second page");
    assert_eq!(first.frame_id, second.frame_id);
    assert_eq!(first.returned_range, RowRange { start: 1, end: 2 });
    assert_eq!(first.next_range, Some(RowRange { start: 3, end: 4 }));
    assert_eq!(second.returned_range, RowRange { start: 3, end: 4 });
    assert!(second.next_range.is_none());
}

#[test]
fn scoped_completeness_requires_a_loaded_task_or_turn_boundary() {
    let session_id = SessionId::new();
    let task_id = TaskId::new();
    let turn_id = TurnId::new();
    let mut app = TuiApp::new(
        ThreadId::new(),
        session_id,
        None,
        false,
        "ready (mock)".to_owned(),
        None,
    );
    app.history_has_more_before = true;
    app.events.push(event(
        300,
        session_id,
        task_id,
        turn_id,
        RuntimeEventType::AssistantMessage,
        json!({"summary": "tail only"}),
    ));

    let (_, task_missing) =
        snapshot_completeness(&app, SnapshotScope::Task, SnapshotPanes::Transcript);
    let (_, turn_missing) =
        snapshot_completeness(&app, SnapshotScope::CurrentTurn, SnapshotPanes::Transcript);
    assert!(task_missing.contains(&"task_history".to_owned()));
    assert!(turn_missing.contains(&"turn_history".to_owned()));

    let mut boundary = event(
        299,
        session_id,
        task_id,
        turn_id,
        RuntimeEventType::TaskCreated,
        json!({}),
    );
    boundary.sequence_no = 299;
    app.events.insert(0, boundary);
    assert_eq!(truncated_history_section(&app, SnapshotScope::Task), None);
    assert_eq!(
        truncated_history_section(&app, SnapshotScope::CurrentTurn),
        None
    );
}
