//! 工具详情只在用户明确打开时占用备用屏幕；聊天草稿与滚动状态由原视图保管。

use super::*;
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::Line,
    widgets::{Paragraph, Wrap},
};

#[derive(Debug)]
pub(crate) struct ToolDetailState {
    pub(crate) id: OperationId,
    projection: OperationProjection,
    scroll: PaneScrollState,
    header_pressed: Option<DetailButton>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetailButton {
    Back,
    Previous,
    Next,
}

pub(crate) fn open_tool_detail(app: &mut TuiApp, id: OperationId) {
    let Some(projection) = history_event_operations(app)
        .into_iter()
        .find(|entry| entry.projection.id() == Some(&id))
        .map(|entry| entry.projection)
    else {
        return;
    };
    app.tool_detail = Some(ToolDetailState {
        id,
        projection,
        scroll: PaneScrollState::default(),
        header_pressed: None,
    });
    app.transcript_pointer = None;
    app.mouse_press = None;
    app.history_tool_press = None;
}

pub(crate) fn close_tool_detail(app: &mut TuiApp) {
    app.tool_detail = None;
    app.transcript_pointer = None;
    app.transcript_screen = None;
    app.invalidate_transcript_layout();
}

fn navigate_tool_detail(app: &mut TuiApp, forward: bool) {
    let Some(current) = app.tool_detail.as_ref().map(|detail| &detail.id) else {
        return;
    };
    let ids = history_event_operations(app)
        .into_iter()
        .filter_map(|entry| entry.projection.id().cloned())
        .collect::<Vec<_>>();
    let Some(index) = ids.iter().position(|id| id == current) else {
        return;
    };
    let next = if forward {
        index.checked_add(1)
    } else {
        index.checked_sub(1)
    };
    if let Some(id) = next.and_then(|index| ids.get(index)) {
        open_tool_detail(app, id.clone());
    }
}

pub(crate) fn draw_tool_detail(frame: &mut Frame<'_>, app: &mut TuiApp) {
    let area = frame.area();
    let id = app.tool_detail.as_ref().expect("tool detail").id.clone();
    if let Some(projection) = history_event_operations(app)
        .into_iter()
        .find(|entry| entry.projection.id() == Some(&id))
        .map(|entry| entry.projection)
    {
        app.tool_detail.as_mut().expect("tool detail").projection = projection;
    }
    let content = Rect::new(
        area.x,
        area.y.saturating_add(2).min(area.bottom()),
        area.width,
        area.height.saturating_sub(3),
    );
    let detail = app.tool_detail.as_mut().expect("tool detail");
    let projection = detail.projection.clone();
    let layout = expanded_tool_layout(app, &projection, content);
    let detail = app.tool_detail.as_mut().expect("tool detail");
    // 默认从调用信息开始阅读；新输出不把正在查看的内容推走。
    detail.scroll.set_row_count(layout.row_count);
    detail.scroll.clamp(usize::from(content.height.max(1)));
    detail.scroll.follow_tail = false;
    let window = layout.visible_window(
        usize::from(content.height),
        detail.scroll.offset_from_bottom,
        None,
    );
    let (logical, local_scroll) = layout.logical_window(window.clone());
    let lines = layout.lines[logical].to_vec();
    let palette = app.palette();
    frame.render_widget(
        Paragraph::new("← Back  Esc     ‹ Prev   Next ›").style(
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Rect::new(area.x, area.y, area.width, area.height.min(1)),
    );
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((ratatui_vertical_scroll(local_scroll, content.height), 0)),
        content,
    );
    if area.height > 2 {
        frame.render_widget(
            Paragraph::new(Line::from(format!(
                "Tool details · {}–{}/{} · scroll ↑↓ · Alt+C copy",
                window.start.saturating_add(1),
                window.end,
                layout.row_count
            )))
            .style(Style::default().fg(palette.muted)),
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
        );
    }
    app.layout.transcript = content;
    app.layout.body = area;
    app.layout.bottom = Rect::default();
    app.layout.body_mode = BodyLayoutMode::Transcript;
    super::transcript_interaction::update_transcript_screen(app, frame.buffer_mut());
}

fn scroll_detail(app: &mut TuiApp, action: TranscriptScrollAction) {
    let rows = usize::from(app.layout.transcript.height.max(1));
    if let Some(detail) = &mut app.tool_detail {
        detail.scroll.scroll(action, rows);
    }
    app.transcript_pointer = None;
}

pub(crate) fn handle_tool_detail_key(key: KeyEvent, app: &mut TuiApp) {
    match key.code {
        KeyCode::Left => navigate_tool_detail(app, false),
        KeyCode::Right => navigate_tool_detail(app, true),
        KeyCode::Esc | KeyCode::Char('q') => close_tool_detail(app),
        KeyCode::Up => scroll_detail(app, TranscriptScrollAction::LineUp),
        KeyCode::Down => scroll_detail(app, TranscriptScrollAction::LineDown),
        KeyCode::PageUp => scroll_detail(app, TranscriptScrollAction::PageUp),
        KeyCode::PageDown | KeyCode::Char(' ') => {
            scroll_detail(app, TranscriptScrollAction::PageDown)
        }
        KeyCode::Home => scroll_detail(app, TranscriptScrollAction::Top),
        KeyCode::End => scroll_detail(app, TranscriptScrollAction::Bottom),
        KeyCode::Char('c')
            if key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
        {
            let text = app
                .transcript_pointer
                .as_ref()
                .filter(|p| p.is_selection())
                .map(transcript_interaction::TranscriptPointer::text)
                .unwrap_or_else(|| {
                    app.tool_detail
                        .as_ref()
                        .expect("tool detail")
                        .projection
                        .item(true)
                        .body
                        .join("\n")
                });
            app.status_message = match copy_to_terminal_clipboard(&text) {
                Ok((bytes, _)) => format!("copied {bytes} bytes"),
                Err(error) => format!("copy failed: {error}"),
            };
            app.transcript_pointer = None;
        }
        _ => {}
    }
}

pub(crate) fn handle_tool_detail_navigation(mouse: MouseEvent, app: &mut TuiApp) -> bool {
    let button = if mouse.row == 0 {
        match mouse.column {
            0..=13 => Some(DetailButton::Back),
            16..=21 => Some(DetailButton::Previous),
            25..=30 => Some(DetailButton::Next),
            _ => None,
        }
    } else {
        None
    };
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) if button.is_some() => {
            app.tool_detail
                .as_mut()
                .expect("tool detail")
                .header_pressed = button;
            true
        }
        MouseEventKind::Up(MouseButton::Left)
            if app
                .tool_detail
                .as_ref()
                .is_some_and(|d| d.header_pressed.is_some()) =>
        {
            let pressed = app
                .tool_detail
                .as_mut()
                .expect("tool detail")
                .header_pressed
                .take();
            if pressed == button {
                match button {
                    Some(DetailButton::Back) => close_tool_detail(app),
                    Some(DetailButton::Previous) => navigate_tool_detail(app, false),
                    Some(DetailButton::Next) => navigate_tool_detail(app, true),
                    None => {}
                }
            }
            true
        }
        MouseEventKind::ScrollUp => {
            scroll_detail(app, TranscriptScrollAction::LineUp);
            true
        }
        MouseEventKind::ScrollDown => {
            scroll_detail(app, TranscriptScrollAction::LineDown);
            true
        }
        _ => false,
    }
}
