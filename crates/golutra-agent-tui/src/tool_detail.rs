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
    sources: Vec<tool_detail_data::DetailSource>,
    load_task: Option<JoinHandle<Vec<String>>>,
    loaded: Option<Vec<String>>,
    query: String,
    searching: bool,
    search_pending: bool,
    search_next: bool,
    match_line: Option<usize>,
    layout_cache: Option<(u16, Arc<TranscriptLayout>)>,
    copy_status: Option<String>,
}

impl Drop for ToolDetailState {
    fn drop(&mut self) {
        if let Some(task) = self.load_task.take() {
            task.abort();
        }
    }
}

pub(crate) async fn poll_data(app: &mut TuiApp, transport: &RuntimeTransport) -> bool {
    let Some(id) = app.tool_detail.as_ref().map(|detail| detail.id.clone()) else {
        return false;
    };
    let sources = tool_detail_data::sources(app, &id);
    let detail = app.tool_detail.as_mut().expect("detail");
    if sources != detail.sources {
        if let Some(task) = detail.load_task.take() {
            task.abort();
        }
        detail.sources = sources.clone();
        detail.loaded = None;
        detail.layout_cache = None;
        if !sources.is_empty() {
            detail.load_task = Some(tokio::spawn(tool_detail_data::load(
                transport.clone(),
                sources,
            )));
        }
    }
    if detail
        .load_task
        .as_ref()
        .is_some_and(|task| task.is_finished())
    {
        let result = detail.load_task.take().expect("load").await;
        detail.loaded = Some(match result {
            Ok(lines) => lines,
            Err(error) => vec![format!("Content load failed: {error}")],
        });
        detail.layout_cache = None;
        detail.search_pending = !detail.query.is_empty();
        return true;
    }
    detail.load_task.is_some()
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
        sources: Vec::new(),
        load_task: None,
        loaded: None,
        query: String::new(),
        searching: false,
        search_pending: false,
        search_next: false,
        match_line: None,
        layout_cache: None,
        copy_status: None,
    });
    app.mouse_press = None;
}

pub(crate) fn close_tool_detail(app: &mut TuiApp) {
    app.tool_detail = None;
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
        let detail = app.tool_detail.as_mut().expect("tool detail");
        if detail.projection != projection {
            detail.projection = projection;
            detail.layout_cache = None;
        }
    }
    let content = Rect::new(
        area.x,
        area.y.saturating_add(2).min(area.bottom()),
        area.width,
        area.height.saturating_sub(3),
    );
    let detail = app.tool_detail.as_ref().expect("tool detail");
    let layout = if let Some((width, layout)) = &detail.layout_cache
        && *width == content.width
    {
        Arc::clone(layout)
    } else {
        let mut projection = detail.projection.clone();
        if let OperationProjection::ToolActivity { details, .. }
        | OperationProjection::FileChange { details, .. } = &mut projection
        {
            if let Some(loaded) = &detail.loaded {
                let header = details
                    .iter()
                    .position(|line| line == "Output" || line == "Diff")
                    .unwrap_or(details.len());
                details.truncate(header);
                details.extend(loaded.iter().cloned());
            } else if detail.load_task.is_some() {
                details.insert(0, "Loading saved content…".to_owned());
            } else {
                let note = if detail.projection.item(false).role == TranscriptRole::Activity {
                    "Live preview; full retained output is saved when execution ends."
                } else {
                    "Saved output unavailable; showing the recorded preview only."
                };
                details.insert(0, note.to_owned());
            }
        }
        let layout = Arc::new(expanded_tool_layout(app, &projection, content));
        app.tool_detail.as_mut().expect("tool detail").layout_cache =
            Some((content.width, Arc::clone(&layout)));
        layout
    };
    let detail = app.tool_detail.as_mut().expect("tool detail");
    // 默认从调用信息开始阅读；新输出不把正在查看的内容推走。
    let first_draw = detail.scroll.row_count == 0;
    detail.scroll.set_row_count(layout.row_count);
    if first_draw {
        detail.scroll.scroll(
            TranscriptScrollAction::Top,
            usize::from(content.height.max(1)),
        );
    }
    detail.scroll.clamp(usize::from(content.height.max(1)));
    detail.scroll.follow_tail = false;
    if detail.search_pending && !detail.query.is_empty() {
        let query = detail.query.to_lowercase();
        let matches = layout
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.to_string().to_lowercase().contains(&query))
            .map(|(i, _)| i)
            .collect::<Vec<_>>();
        let found = if detail.search_next {
            matches
                .iter()
                .copied()
                .find(|i| Some(*i) > detail.match_line)
                .or_else(|| matches.first().copied())
        } else {
            matches.first().copied()
        };
        detail.match_line = found;
        if let Some(row) = found.and_then(|i| layout.visual_start_for_line(i)) {
            detail.scroll.offset_from_bottom = layout
                .row_count
                .saturating_sub(row + usize::from(content.height));
        }
        detail.search_pending = false;
    }
    let window = layout.visible_window(
        usize::from(content.height),
        detail.scroll.offset_from_bottom,
        None,
    );
    let (logical, local_scroll) = layout.logical_window(window.clone());
    let mut lines = layout.lines[logical].to_vec();
    if !detail.query.is_empty() {
        for line in &mut lines {
            if line
                .to_string()
                .to_lowercase()
                .contains(&detail.query.to_lowercase())
            {
                *line = line
                    .clone()
                    .style(Style::default().add_modifier(Modifier::REVERSED));
            }
        }
    }
    let search_hint = if detail.searching {
        format!("/{}▏ · Enter search · Esc cancel", detail.query)
    } else if let Some(status) = &detail.copy_status {
        status.clone()
    } else if !detail.query.is_empty() {
        format!(
            "/{} · {} · n next",
            detail.query,
            if detail.match_line.is_some() {
                "match"
            } else {
                "no match"
            }
        )
    } else {
        "/ search · Alt+C copy".to_owned()
    };
    let palette = app.palette();
    frame.render_widget(
        Paragraph::new("Ctrl+O / Esc back     ← previous tool   → next tool").style(
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
                "Tool details · {}–{}/{} · {search_hint}",
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
}

fn scroll_detail(app: &mut TuiApp, action: TranscriptScrollAction) {
    let rows = usize::from(app.layout.transcript.height.max(1));
    if let Some(detail) = &mut app.tool_detail {
        detail.scroll.scroll(action, rows);
    }
}

pub(crate) fn handle_tool_detail_key(key: KeyEvent, app: &mut TuiApp) {
    if key.code == KeyCode::Char('o') && key.modifiers.contains(KeyModifiers::CONTROL) {
        close_tool_detail(app);
        return;
    }
    let detail = app.tool_detail.as_mut().expect("detail");
    if detail.searching {
        match key.code {
            KeyCode::Esc => {
                detail.searching = false;
                detail.query.clear();
                detail.match_line = None;
            }
            KeyCode::Enter => {
                detail.searching = false;
                detail.search_pending = true;
                detail.search_next = false;
            }
            KeyCode::Backspace => {
                detail.query.pop();
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                detail.query.push(ch);
                detail.copy_status = None;
            }
            _ => {}
        }
        return;
    }
    match key.code {
        KeyCode::Char('/') => {
            detail.searching = true;
            detail.query.clear();
        }
        KeyCode::Char('n') => {
            detail.search_pending = true;
            detail.search_next = true;
        }
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
            let detail = app.tool_detail.as_ref().expect("tool detail");
            let text = detail
                .loaded
                .as_ref()
                .map(|lines| lines.join("\n"))
                .unwrap_or_else(|| detail.projection.item(true).body.join("\n"));
            app.status_message = match copy_to_terminal_clipboard(&text) {
                Ok((bytes, true)) => format!("copied {bytes} bytes (clipboard limit reached)"),
                Ok((bytes, false)) => format!("copied {bytes} bytes"),
                Err(error) => format!("copy failed: {error}"),
            };
            app.tool_detail.as_mut().expect("tool detail").copy_status =
                Some(app.status_message.clone());
        }
        _ => {}
    }
}

pub(crate) fn paste_query(app: &mut TuiApp, pasted: &str) {
    if let Some(detail) = &mut app.tool_detail
        && detail.searching
    {
        detail
            .query
            .extend(pasted.chars().filter(|ch| !ch.is_control()));
        detail.copy_status = None;
    }
}
