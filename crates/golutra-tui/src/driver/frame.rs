use std::collections::HashSet;

use golutra_client::redact_runtime_value;
use golutra_core::{ArtifactId, EvidenceId, TaskId, TurnId};
use golutra_protocol::{
    DebugProjection, RowRange, RuntimeEvent, RuntimeEventType, SnapshotPanes, SnapshotScope,
    TUI_DRIVER_MAX_RETURNED_ROWS, TuiFrame, TuiFrameCell, TuiFrameLine, TuiFramePane, TuiHitPane,
    TuiHitRegion,
};
use ratatui::{buffer::Buffer, layout::Rect};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::super::{
    TuiApp, UiLayoutSnapshot, UiMousePress, developer_facts_toggle_hit_rect, display_width,
    overlay_mouse_regions, transcript_toggle_regions,
};

pub(super) fn scoped_runtime_events(
    events: &[RuntimeEvent],
    scope: SnapshotScope,
) -> Vec<RuntimeEvent> {
    let (task_id, turn_id) = latest_task_and_turn(events);
    events
        .iter()
        .filter(|event| event_in_scope(event, scope, task_id, turn_id))
        .cloned()
        .map(|mut event| {
            redact_runtime_value(&mut event.payload);
            event
        })
        .collect()
}

pub(super) fn redacted_ui_text(text: &str) -> String {
    let mut value = Value::String(text.to_owned());
    redact_runtime_value(&mut value);
    value
        .as_str()
        .map_or_else(|| "<redacted-secret>".to_owned(), ToOwned::to_owned)
}

pub(super) fn scoped_debug_projection(
    mut projection: DebugProjection,
    scope: SnapshotScope,
) -> miette::Result<DebugProjection> {
    let (task_id, turn_id) = latest_task_and_turn(&projection.events);
    projection
        .events
        .retain(|event| event_in_scope(event, scope, task_id, turn_id));
    for event in &mut projection.events {
        redact_runtime_value(&mut event.payload);
    }

    if matches!(scope, SnapshotScope::CurrentTurn) {
        projection
            .busy_policy_decisions
            .retain(|decision| decision.affected_turn_id == turn_id);
        projection
            .artifacts
            .retain(|artifact| artifact.turn_id == turn_id);
        projection
            .loop_decisions
            .retain(|decision| Some(decision.turn_id) == turn_id);
    }
    if matches!(scope, SnapshotScope::CurrentTurn | SnapshotScope::Task) {
        if let Some(task_id) = task_id {
            projection.task_id = Some(task_id);
            if projection
                .verification
                .as_ref()
                .is_some_and(|verification| verification.task_id != task_id)
            {
                projection.verification = None;
            }
            projection
                .loop_decisions
                .retain(|decision| decision.task_id == task_id);
            projection
                .post_task_jobs
                .retain(|job| job.task_id == task_id);
        }
        retain_referenced_facts(&mut projection);
    }
    projection.event_window.start_cursor = projection.events.first().map(|event| event.sequence_no);
    projection.event_window.end_cursor = projection.events.last().map(|event| event.sequence_no);
    projection.event_window.has_more_before = false;
    redact_debug_projection(projection)
}

fn redact_debug_projection(projection: DebugProjection) -> miette::Result<DebugProjection> {
    let mut value = serde_json::to_value(&projection)
        .map_err(|error| miette::miette!("redaction_failed: {error}"))?;
    redact_runtime_value(&mut value);
    serde_json::from_value(value).map_err(|error| miette::miette!("redaction_failed: {error}"))
}

fn retain_referenced_facts(projection: &mut DebugProjection) {
    let tool_ids = projection
        .events
        .iter()
        .filter_map(|event| event.payload.get("envelope"))
        .filter_map(|envelope| envelope.get("tool_call_id"))
        .filter_map(Value::as_str)
        .collect::<HashSet<_>>();
    projection
        .tool_results
        .retain(|result| tool_ids.contains(result.tool_call_id.to_string().as_str()));

    let mut artifact_ids = projection
        .tool_results
        .iter()
        .filter_map(|result| result.raw_artifact_ref)
        .collect::<HashSet<ArtifactId>>();
    let mut evidence_ids = projection
        .tool_results
        .iter()
        .flat_map(|result| result.evidence_refs.iter().copied())
        .collect::<HashSet<EvidenceId>>();
    if let Some(verification) = &projection.verification {
        evidence_ids.extend(verification.evidence_refs.iter().copied());
    }
    for decision in &projection.loop_decisions {
        evidence_ids.extend(decision.evidence_refs.iter().copied());
    }
    projection
        .evidence
        .retain(|evidence| evidence_ids.contains(&evidence.evidence_id));
    for evidence in &projection.evidence {
        artifact_ids.extend(evidence.artifact_refs.iter().copied());
    }
    if !artifact_ids.is_empty() {
        projection
            .artifacts
            .retain(|artifact| artifact_ids.contains(&artifact.artifact_id));
    }
}

fn latest_task_and_turn(events: &[RuntimeEvent]) -> (Option<TaskId>, Option<TurnId>) {
    let task_id = events.iter().rev().find_map(|event| event.task_id);
    let turn_id = task_id
        .and_then(|task_id| {
            events
                .iter()
                .rev()
                .find(|event| event.task_id == Some(task_id))
                .and_then(|event| event.turn_id)
        })
        .or_else(|| events.iter().rev().find_map(|event| event.turn_id));
    (task_id, turn_id)
}

fn event_in_scope(
    event: &RuntimeEvent,
    scope: SnapshotScope,
    task_id: Option<TaskId>,
    turn_id: Option<TurnId>,
) -> bool {
    match scope {
        SnapshotScope::Screen | SnapshotScope::Session => true,
        SnapshotScope::Task => task_id.is_some() && event.task_id == task_id,
        SnapshotScope::CurrentTurn => {
            turn_id.is_some() && event.turn_id == turn_id && event.task_id == task_id
        }
    }
}

pub(super) fn snapshot_area(
    panes: SnapshotPanes,
    layout: UiLayoutSnapshot,
    width: u16,
    height: u16,
) -> miette::Result<Rect> {
    match panes {
        SnapshotPanes::Transcript => Ok(layout.transcript),
        SnapshotPanes::Developer => layout.developer.ok_or_else(|| {
            miette::miette!("developer_unavailable: developer pane was not rendered")
        }),
        SnapshotPanes::ResponseAndDeveloper => {
            let developer = layout.developer.ok_or_else(|| {
                miette::miette!("developer_unavailable: developer pane was not rendered")
            })?;
            Ok(Rect::new(
                layout.transcript.x,
                layout.transcript.y,
                developer.right().saturating_sub(layout.transcript.x),
                layout.transcript.height.max(developer.height),
            ))
        }
        SnapshotPanes::FullScreen => Ok(Rect::new(0, 0, width, height)),
    }
}

pub(super) fn frame_lines(buffer: &Buffer, area: Rect, panes: SnapshotPanes) -> Vec<TuiFrameLine> {
    (0..area.height)
        .map(|row| {
            let y = area.y.saturating_add(row);
            let mut text = String::new();
            let mut column = 0;
            while column < area.width {
                let x = area.x.saturating_add(column);
                if let Some(cell) = buffer.cell((x, y)) {
                    text.push_str(cell.symbol());
                    let symbol_width = display_width(cell.symbol()).max(1);
                    column = column.saturating_add(u16::try_from(symbol_width).unwrap_or(u16::MAX));
                } else {
                    column = column.saturating_add(1);
                }
            }
            let text = text.trim_end_matches(' ').to_owned();
            TuiFrameLine {
                row: u32::from(row) + 1,
                display_width: u16::try_from(display_width(&text)).unwrap_or(u16::MAX),
                text,
                pane: pane_name(panes),
            }
        })
        .collect()
}

pub(super) fn frame_cells(buffer: &Buffer, area: Rect, panes: SnapshotPanes) -> Vec<TuiFrameCell> {
    let mut cells = Vec::with_capacity(usize::from(area.width) * usize::from(area.height));
    for row in 0..area.height {
        let mut column = 0;
        while column < area.width {
            let x = area.x.saturating_add(column);
            let y = area.y.saturating_add(row);
            let Some(cell) = buffer.cell((x, y)) else {
                column = column.saturating_add(1);
                continue;
            };
            cells.push(TuiFrameCell {
                row: row + 1,
                column: column + 1,
                symbol: cell.symbol().to_owned(),
                pane: pane_name(panes),
                foreground: cell.fg.to_string(),
                background: cell.bg.to_string(),
                modifiers: format!("{:?}", cell.modifier),
            });
            let symbol_width = display_width(cell.symbol()).max(1);
            column = column.saturating_add(u16::try_from(symbol_width).unwrap_or(u16::MAX));
        }
    }
    cells
}

pub(super) fn frame_hit_regions(
    layout: UiLayoutSnapshot,
    area: Rect,
    app: &TuiApp,
) -> Vec<TuiHitRegion> {
    let mut regions = Vec::new();
    push_hit_region(
        &mut regions,
        "transcript",
        TuiHitPane::Transcript,
        layout.transcript,
        area,
    );
    let transcript_area = layout.transcript.intersection(area);
    for (id, region) in transcript_toggle_regions(app, transcript_area) {
        push_hit_region(&mut regions, &id, TuiHitPane::Transcript, region, area);
    }
    push_hit_region(
        &mut regions,
        "composer",
        TuiHitPane::Bottom,
        layout.bottom,
        area,
    );
    if layout.hit_test(layout.transcript.x, layout.transcript.y, app)
        == super::super::UiHitTarget::Overlay
    {
        push_hit_region(
            &mut regions,
            "overlay",
            TuiHitPane::Overlay,
            layout.transcript,
            area,
        );
        for region in overlay_mouse_regions(layout.transcript, app) {
            push_hit_region(
                &mut regions,
                &overlay_region_id(region.press),
                TuiHitPane::Overlay,
                region.area,
                area,
            );
        }
    }
    if let Some(developer) = layout.developer {
        push_hit_region(
            &mut regions,
            "developer",
            TuiHitPane::Developer,
            developer,
            area,
        );
        push_hit_region(
            &mut regions,
            "developer_facts_toggle",
            TuiHitPane::Developer,
            developer_facts_toggle_hit_rect(developer),
            area,
        );
    }
    regions
}

fn overlay_region_id(press: UiMousePress) -> String {
    match press {
        UiMousePress::Auth(index) => format!("auth_option_{index}"),
        UiMousePress::Resume(index) => format!("resume_item_{index}"),
        UiMousePress::Queue(index) => format!("queue_item_{index}"),
        UiMousePress::Approval(choice) => format!("approval_{}", region_slug(choice.label())),
        UiMousePress::QuestionOption { question, option } => {
            format!("question_{question}_option_{option}")
        }
        UiMousePress::QuestionFreeText { question } => {
            format!("question_{question}_free_text")
        }
        UiMousePress::QuestionSubmit => "question_submit".to_owned(),
        UiMousePress::Dashboard(tab) => format!("dashboard_{}", region_slug(tab.label())),
        UiMousePress::Settings(row) => format!("settings_{}", row.index()),
        UiMousePress::Help(topic) => format!("help_{}", region_slug(topic.label())),
    }
}

fn region_slug(label: &str) -> String {
    let mut slug = String::with_capacity(label.len());
    let mut separator = false;
    for character in label.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            slug.push(character);
            separator = false;
        } else if !separator && !slug.is_empty() {
            slug.push('_');
            separator = true;
        }
    }
    while slug.ends_with('_') {
        slug.pop();
    }
    slug
}

fn push_hit_region(
    regions: &mut Vec<TuiHitRegion>,
    id: &str,
    pane: TuiHitPane,
    region: Rect,
    visible: Rect,
) {
    let clipped = region.intersection(visible);
    if clipped.width == 0 || clipped.height == 0 {
        return;
    }
    regions.push(TuiHitRegion {
        id: id.to_owned(),
        pane,
        x: clipped.x,
        y: clipped.y,
        width: clipped.width,
        height: clipped.height,
    });
}

fn pane_name(panes: SnapshotPanes) -> TuiFramePane {
    match panes {
        SnapshotPanes::Transcript => TuiFramePane::Transcript,
        SnapshotPanes::Developer => TuiFramePane::Developer,
        SnapshotPanes::ResponseAndDeveloper => TuiFramePane::ResponseAndDeveloper,
        SnapshotPanes::FullScreen => TuiFramePane::Screen,
    }
}

pub(super) fn panes_include_developer(panes: SnapshotPanes, debug_mode: bool) -> bool {
    matches!(
        panes,
        SnapshotPanes::Developer | SnapshotPanes::ResponseAndDeveloper
    ) || (matches!(panes, SnapshotPanes::FullScreen) && debug_mode)
}

pub(super) fn snapshot_completeness(
    app: &TuiApp,
    scope: SnapshotScope,
    panes: SnapshotPanes,
) -> (bool, Vec<String>) {
    let mut missing = Vec::new();
    if app.projection.is_none() {
        missing.push("user_projection".to_owned());
    }
    if let Some(section) = truncated_history_section(app, scope) {
        missing.push(section.to_owned());
    }
    if panes_include_developer(panes, app.debug_mode) {
        match &app.developer_projection {
            Some(projection) => {
                missing.extend(projection.missing_sections.iter().cloned());
                missing.extend(
                    projection
                        .retention_losses
                        .iter()
                        .map(|loss| format!("retention:{loss}")),
                );
                if !projection.trace_complete {
                    missing.push("trace_incomplete".to_owned());
                }
            }
            None => missing.push("debug_projection".to_owned()),
        }
    }
    missing.sort();
    missing.dedup();
    (missing.is_empty(), missing)
}

fn truncated_history_section(app: &TuiApp, scope: SnapshotScope) -> Option<&'static str> {
    if !app.history_has_more_before {
        return None;
    }
    if matches!(scope, SnapshotScope::Session) {
        return Some("session_history");
    }
    if matches!(scope, SnapshotScope::Screen) {
        return None;
    }

    let (latest_task_id, latest_turn_id) = latest_task_and_turn(&app.events);
    match scope {
        SnapshotScope::Task => {
            let task_id = app.task_id.or(latest_task_id);
            let has_boundary = task_id.is_some_and(|task_id| {
                app.events.iter().any(|event| {
                    event.task_id == Some(task_id)
                        && event.event_type == RuntimeEventType::TaskCreated
                })
            });
            (!has_boundary).then_some("task_history")
        }
        SnapshotScope::CurrentTurn => {
            let has_boundary = latest_turn_id.is_some_and(|turn_id| {
                app.events.iter().any(|event| {
                    event.turn_id == Some(turn_id)
                        && matches!(
                            event.event_type,
                            RuntimeEventType::TaskCreated
                                | RuntimeEventType::TurnQueued
                                | RuntimeEventType::TurnStarted
                        )
                })
            });
            (!has_boundary).then_some("turn_history")
        }
        SnapshotScope::Session | SnapshotScope::Screen => None,
    }
}

pub(super) fn frame_digest(frame: &TuiFrame) -> miette::Result<String> {
    let mut canonical = frame.clone();
    canonical.frame_id.clear();
    canonical.next_range = None;
    canonical.returned_range = RowRange {
        start: u32::from(canonical.total_rows > 0),
        end: canonical.total_rows,
    };
    let bytes = serde_json::to_vec(&canonical)
        .map_err(|error| miette::miette!("frame_digest_failed: {error}"))?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

pub(super) fn slice_frame(mut frame: TuiFrame, rows: Option<RowRange>) -> miette::Result<TuiFrame> {
    let requested = rows.unwrap_or(RowRange {
        start: u32::from(frame.total_rows > 0),
        end: frame.total_rows,
    });
    if frame.total_rows == 0 {
        frame.returned_range = RowRange { start: 0, end: 0 };
        frame.lines.clear();
        frame.cells = frame.cells.map(|_| Vec::new());
        frame.next_range = None;
        return Ok(frame);
    }
    if requested.start == 0
        || requested.end < requested.start
        || requested.end > frame.total_rows
        || requested
            .end
            .saturating_sub(requested.start)
            .saturating_add(1)
            > TUI_DRIVER_MAX_RETURNED_ROWS
    {
        return Err(miette::miette!(
            "invalid_rows: requested rows must be a 1-based inclusive range within the frozen frame and contain at most {TUI_DRIVER_MAX_RETURNED_ROWS} rows"
        ));
    }
    frame
        .lines
        .retain(|line| line.row >= requested.start && line.row <= requested.end);
    if let Some(cells) = &mut frame.cells {
        cells.retain(|cell| {
            u32::from(cell.row) >= requested.start && u32::from(cell.row) <= requested.end
        });
    }
    frame.returned_range = requested;
    frame.next_range = (requested.end < frame.total_rows).then(|| RowRange {
        start: requested.end + 1,
        end: frame
            .total_rows
            .min(requested.end.saturating_add(TUI_DRIVER_MAX_RETURNED_ROWS)),
    });
    Ok(frame)
}

#[cfg(test)]
mod tests;
