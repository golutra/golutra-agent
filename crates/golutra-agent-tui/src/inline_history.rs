//! 主聊天的终端历史归档、语义行定位与 debug 时间线排版。

use std::collections::{HashMap, HashSet};
use std::{io, sync::Arc};

use golutra_agent_core::EventId;
use golutra_agent_core::SessionId;
use ratatui::backend::Backend;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Paragraph, Widget, Wrap},
};
use unicode_segmentation::UnicodeSegmentation;

use super::*;

const MIN_INLINE_BOTTOM_ROWS: u16 = 3;
const HISTORY_OMISSION_MARKER: &str = "…";

#[derive(Debug, Clone)]
pub(crate) struct HistoryDisplayRow {
    line: Line<'static>,
    pub(crate) tool_id: Option<OperationId>,
}

fn unpadded_history_line(mut spans: Vec<Span<'static>>) -> Line<'static> {
    // 去掉物理补齐空格时保留整行底色，缩放重排后 diff 仍覆盖新行宽。
    let background = spans
        .last()
        .and_then(|span| span.style.bg)
        .filter(|color| *color != ratatui::style::Color::Reset);
    // 缓存的是实际文字而非 Buffer 的补齐空格；缩窄时补齐格不能再次折成空白行。
    while let Some(last) = spans.last_mut() {
        last.content = last.content.trim_end().to_owned().into();
        if !last.content.is_empty() {
            break;
        }
        spans.pop();
    }
    let line = Line::from(spans);
    if let Some(color) = background {
        line.style(Style::default().bg(color))
    } else {
        line
    }
}

fn retain_screen_rows(rows: &mut Vec<HistoryDisplayRow>, screen_height: u16) {
    let remove = rows.len().saturating_sub(usize::from(screen_height));
    rows.drain(..remove);
}

fn append_tool_fragment(
    tail: &mut super::transcript_spacing::TranscriptTail,
    lines: &mut Vec<Line<'static>>,
    fragment: &[Line<'static>],
    event_id: Option<EventId>,
    tool_id: Option<&OperationId>,
    ranges: &mut Vec<(std::ops::Range<usize>, OperationId)>,
) {
    tail.append(lines, fragment, event_id);
    if let Some(id) = tool_id
        && !fragment.is_empty()
    {
        ranges.push((
            lines.len().saturating_sub(fragment.len())..lines.len(),
            id.clone(),
        ));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InlineHistoryMode {
    Transcript,
    Developer { expanded: bool },
    DebugSplit { expanded: bool },
}

impl InlineHistoryMode {
    fn from_app(app: &TuiApp) -> Self {
        if !app.debug_mode {
            return Self::Transcript;
        }
        match app.body_view_mode {
            BodyViewMode::Transcript => Self::Transcript,
            BodyViewMode::Developer => Self::Developer {
                expanded: app.developer_observations_expanded,
            },
            BodyViewMode::Auto | BodyViewMode::Split => Self::DebugSplit {
                expanded: app.developer_observations_expanded,
            },
        }
    }
}

#[derive(Debug, Clone)]
struct RenderedHistoryEntry {
    tool_id: Option<OperationId>,
    event_ids: Vec<EventId>,
    lines: Vec<Line<'static>>,
    stable_line_count: usize,
    commit_event: bool,
    source_prefix: Option<String>,
}

/// 本地命令没有 runtime 事件；保留语义内容和归档锚点，窗口重排时不能丢掉 /status。
#[derive(Debug, Clone)]
struct LocalHistoryEntry {
    anchor: Option<EventId>,
    source_prefix: Option<String>,
    items: Vec<TranscriptItem>,
    emitted: bool,
}

impl LocalHistoryEntry {
    fn offset_in(&self, app: &TuiApp, entry: &RenderedHistoryEntry, width: u16) -> usize {
        self.source_prefix
            .as_ref()
            .map_or(entry.lines.len(), |prefix| {
                let rendered = render_operation_projection_lines(
                    app,
                    vec![message_projection(TranscriptItem {
                        role: TranscriptRole::Assistant,
                        title: "Golutra".to_owned(),
                        body: vec![prefix.clone()],
                    })],
                    width,
                );
                super::stream_commit::stable_rendered_prefix(&entry.lines, &rendered)
            })
    }
}

impl RenderedHistoryEntry {
    fn is_committed(&self, committed: &HashSet<EventId>) -> bool {
        self.event_ids.iter().all(|id| committed.contains(id))
    }

    fn is_partially_committed(&self, committed: &HashSet<EventId>) -> bool {
        self.event_ids.iter().any(|id| committed.contains(id)) && !self.is_committed(committed)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct InlineHistoryState {
    native_scrollback: bool,
    display_rows: Arc<Vec<HistoryDisplayRow>>,
    session_id: SessionId,
    generation: u64,
    mode: InlineHistoryMode,
    rendered_width: u16,
    initialized: bool,
    header_emitted: bool,
    rebuild_after_replay: bool,
    committed_event_ids: HashSet<EventId>,
    committed_stream_lines: HashMap<EventId, usize>,
    // 只保留活动流已写入的渲染前缀；最终正文或后置 Markdown 定义可能修订它。
    committed_stream_prefixes: HashMap<EventId, Arc<[Line<'static>]>>,
    committed_stream_sources: HashMap<EventId, String>,
    local_entries: Vec<LocalHistoryEntry>,
    last_anchor: Option<EventId>,
    last_source_prefix: Option<String>,
    tail: super::transcript_spacing::TranscriptTail,
}

impl InlineHistoryState {
    /// 只重画缩放前主屏可见的应用行；已进入 scrollback 的正文和 shell 前缀均不清除。
    pub(crate) fn resize_visible_history(
        &mut self,
        terminal: &mut InteractiveTerminal,
        previous: Rect,
        size: ratatui::layout::Size,
    ) -> io::Result<()> {
        let count = self.display_rows.len().min(usize::from(previous.y));
        let start = previous
            .y
            .saturating_sub(count as u16)
            .min(size.height.saturating_sub(1));
        let visible = self.display_rows[self.display_rows.len() - count..].to_vec();
        let mut rows = Vec::new();
        for row in visible {
            rows.extend(
                wrapped_history_rows(vec![row.line], size.width)
                    .into_iter()
                    .map(|spans| HistoryDisplayRow {
                        line: unpadded_history_line(spans),
                        tool_id: row.tool_id.clone(),
                    }),
            );
        }
        clear_inline_region(terminal, start, previous.height.min(size.height))?;
        insert_history_lines(
            terminal,
            rows.iter().map(|row| row.line.clone()).collect(),
            size.width,
        )?;
        let display = Arc::make_mut(&mut self.display_rows);
        display.truncate(display.len() - count);
        display.extend(rows);
        retain_screen_rows(display, size.height);
        Ok(())
    }

    pub(crate) fn new(session_id: SessionId) -> Self {
        Self {
            native_scrollback: false,
            display_rows: Arc::new(Vec::new()),
            session_id,
            generation: 0,
            mode: InlineHistoryMode::Transcript,
            rendered_width: 0,
            initialized: false,
            header_emitted: false,
            rebuild_after_replay: false,
            committed_event_ids: HashSet::new(),
            committed_stream_lines: HashMap::new(),
            committed_stream_prefixes: HashMap::new(),
            committed_stream_sources: HashMap::new(),
            local_entries: Vec::new(),
            last_anchor: None,
            last_source_prefix: None,
            tail: super::transcript_spacing::TranscriptTail::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn flush<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        app: &mut TuiApp,
    ) -> io::Result<bool> {
        self.flush_with_rebuild(terminal, app, clear_history_terminal)
    }

    #[cfg(test)]
    pub(crate) fn flush_with_rebuild_for_test<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        app: &mut TuiApp,
        rebuild_terminal: impl FnMut(&mut Terminal<B>) -> io::Result<()>,
    ) -> io::Result<bool> {
        self.flush_with_rebuild(terminal, app, rebuild_terminal)
    }

    pub(crate) fn flush_interactive(
        &mut self,
        terminal: &mut InteractiveTerminal,
        app: &mut TuiApp,
    ) -> io::Result<bool> {
        self.native_scrollback = true;
        self.flush_with_callbacks(
            terminal,
            app,
            |_terminal| Ok(()),
            sync_inline_viewport_height,
        )
    }

    #[cfg(test)]
    fn flush_with_rebuild<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        app: &mut TuiApp,
        rebuild_terminal: impl FnMut(&mut Terminal<B>) -> io::Result<()>,
    ) -> io::Result<bool> {
        self.flush_with_callbacks(terminal, app, rebuild_terminal, |_, _| Ok(()))
    }

    fn flush_with_callbacks<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        app: &mut TuiApp,
        rebuild_terminal: impl FnMut(&mut Terminal<B>) -> io::Result<()>,
        prepare_insert: impl FnMut(&mut Terminal<B>, &TuiApp) -> io::Result<()>,
    ) -> io::Result<bool> {
        // 归档标记与本地命令只有在终端写入成功后才能提交；失败不能吞掉待显示内容。
        let mut next = self.clone();
        let previous_history = app.transcript.history.clone();
        let previous_commands = app.command_messages.clone();
        match next.flush_prepared(terminal, app, rebuild_terminal, prepare_insert) {
            Ok(changed) => {
                *self = next;
                Ok(changed)
            }
            Err(error) => {
                app.transcript.history = previous_history;
                app.command_messages = previous_commands;
                app.invalidate_transcript_layout();
                Err(error)
            }
        }
    }

    fn flush_prepared<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        app: &mut TuiApp,
        mut rebuild_terminal: impl FnMut(&mut Terminal<B>) -> io::Result<()>,
        mut prepare_insert: impl FnMut(&mut Terminal<B>, &TuiApp) -> io::Result<()>,
    ) -> io::Result<bool> {
        // 归档发生在普通帧绘制之前；独立调用也必须先同步尺寸，使换行与插入共享同一物理宽度。
        terminal.autoresize()?;
        let mode = InlineHistoryMode::from_app(app);
        let buffer_area = terminal.current_buffer_mut().area;
        let width = buffer_area.width.max(1);
        // 原生历史只追加；布局刷新不能清除 shell 历史，也不能重复提交已显示消息。
        if self.native_scrollback && self.session_id == app.session_id {
            app.set_inline_history_committed_event_ids(self.committed_event_ids.clone());
            app.set_inline_history_committed_stream_lines(self.committed_stream_lines.clone());
            app.transcript.history.tail = self.tail.clone();
        }
        let layout_width = if self.native_scrollback
            && self.session_id == app.session_id
            && !self.committed_stream_lines.is_empty()
        {
            self.rendered_width.max(1)
        } else {
            width
        };
        app.transcript.history.native_render_width = self.native_scrollback.then_some(layout_width);
        // 对照 Codex：viewport 高度变化（slash 弹层、overlay 进出、流式尾巴）不能重建 scrollback。
        let identity_changed = self.initialized
            && (self.session_id != app.session_id
                || (!self.native_scrollback
                    && (self.generation != app.transcript.history.replay_generation
                        || self.mode != mode
                        || self.rendered_width != width)));
        let clear_previous_history =
            identity_changed && (self.header_emitted || !self.committed_event_ids.is_empty());

        let mut history_cleared = false;
        if clear_previous_history {
            rebuild_terminal(terminal)?;
            history_cleared = true;
        }
        if !self.initialized || identity_changed {
            self.tail = super::transcript_spacing::TranscriptTail::default();
            if self.session_id != app.session_id {
                self.local_entries.clear();
            }
            for entry in &mut self.local_entries {
                entry.emitted = false;
            }
            self.last_anchor = None;
            self.last_source_prefix = None;
            self.session_id = app.session_id;
            self.generation = app.transcript.history.replay_generation;
            self.mode = mode;
            self.rendered_width = layout_width;
            self.initialized = true;
            self.header_emitted = false;
            self.committed_event_ids.clear();
            self.committed_stream_lines.clear();
            self.committed_stream_prefixes.clear();
            self.committed_stream_sources.clear();
            app.set_inline_history_committed_event_ids(HashSet::new());
            app.set_inline_history_committed_stream_lines(HashMap::new());
        }

        let source_ready = app.transcript.history.replay_ready
            && (!matches!(
                mode,
                InlineHistoryMode::Developer { .. } | InlineHistoryMode::DebugSplit { .. }
            ) || app.developer_projection.is_some()
                || app.developer_error.is_some());
        if !source_ready {
            // A resume/debug reload can render a provisional projection while canonical events
            // are still loading. Rebuild once the source is ready so that frame is not folded
            // into the next terminal scrollback insertion.
            self.rebuild_after_replay = true;
            return Ok(history_cleared);
        }
        if self.rebuild_after_replay && !self.native_scrollback {
            if !history_cleared {
                rebuild_terminal(terminal)?;
                history_cleared = true;
            }
            self.rebuild_after_replay = false;
        }

        app.transcript.history.command_anchors = self
            .local_entries
            .iter()
            .filter_map(|entry| entry.anchor)
            .collect();
        self.rendered_width = layout_width;
        let mut history_entries = rendered_history_entries(app, layout_width, mode);
        let committable_ids = history_entries
            .iter()
            .flat_map(|entry| entry.event_ids.iter().copied())
            .collect::<HashSet<_>>();
        let grouping_changed = !matches!(mode, InlineHistoryMode::Transcript)
            && history_entries.iter().any(|entry| {
                entry.commit_event && entry.is_partially_committed(&self.committed_event_ids)
            });
        let stream_prefix_changed = history_entries.iter().any(|entry| {
            entry
                .event_ids
                .iter()
                .any(|id| self.stream_prefix_revised(entry, id))
        });
        if !self.native_scrollback
            && ((!matches!(mode, InlineHistoryMode::Transcript)
                && !self.committed_event_ids.is_subset(&committable_ids))
                || grouping_changed
                || stream_prefix_changed)
        {
            if !history_cleared {
                rebuild_terminal(terminal)?;
                history_cleared = true;
            }
            self.header_emitted = false;
            for entry in &mut self.local_entries {
                entry.emitted = false;
            }
            self.tail = super::transcript_spacing::TranscriptTail::default();
            self.last_anchor = None;
            self.last_source_prefix = None;
            self.committed_event_ids.clear();
            self.committed_stream_lines.clear();
            self.committed_stream_prefixes.clear();
            self.committed_stream_sources.clear();
            app.set_inline_history_committed_event_ids(HashSet::new());
            app.set_inline_history_committed_stream_lines(HashMap::new());
            // 取消旧提交边界后重新投影，不能沿用受旧分组边界影响的条目。
            history_entries = rendered_history_entries(app, width, mode);
        }
        let native_stream_revised = self.native_scrollback && stream_prefix_changed;
        if native_stream_revised {
            // 已进入终端历史的流式正文不可擦除；真正的最终修订以明确标记追加，不能按旧行数截掉新答案。
            for entry in &history_entries {
                for id in &entry.event_ids {
                    if self.stream_prefix_revised(entry, id) {
                        self.committed_stream_prefixes.remove(id);
                        self.committed_stream_sources.remove(id);
                        self.committed_stream_lines.remove(id);
                    }
                }
            }
        }
        self.committed_stream_prefixes
            .retain(|id, _| committable_ids.contains(id));
        self.committed_stream_sources
            .retain(|id, _| committable_ids.contains(id));
        if history_cleared || !self.header_emitted {
            self.display_rows = Arc::new(Vec::new());
        }
        let mut lines = Vec::new();
        let mut tool_ranges = Vec::new();
        let emit_header = !self.header_emitted;
        if emit_header {
            lines.extend(session_history_lines(app, width));
            let fact_lines = match mode {
                InlineHistoryMode::Developer { expanded: true } => {
                    developer_fact_history_lines(app, width)
                }
                InlineHistoryMode::DebugSplit { expanded: true } => {
                    let (_, developer_width) = debug_pane_widths(width);
                    debug_split_history_lines(
                        Vec::new(),
                        developer_fact_history_lines(app, developer_width),
                        width,
                    )
                }
                _ => Vec::new(),
            };
            if !fact_lines.is_empty() {
                lines.extend(fact_lines);
                lines.push(Line::default());
            }
            if let Some(last) = lines.last() {
                self.tail.observe(last, None);
            }
        }

        if native_stream_revised {
            self.tail.append(
                &mut lines,
                &[Line::from(
                    "• Updated response (replaces earlier streamed text)",
                )],
                None,
            );
        }
        self.append_event_lines(
            app,
            history_entries,
            layout_width,
            &mut lines,
            &mut tool_ranges,
        );
        self.append_local_messages(app, width, &mut lines);

        app.set_inline_history_committed_event_ids(self.committed_event_ids.clone());
        app.set_inline_history_committed_stream_lines(self.committed_stream_lines.clone());
        if app.transcript.history.tail != self.tail {
            app.transcript.history.tail = self.tail.clone();
            app.invalidate_transcript_layout();
        }
        let changed = !lines.is_empty();
        if changed {
            // 已归档部分不再占用活动区；先按剩余尾部确定高度，再插入历史。
            // 否则长代码块会先被旧的整屏活动区全部挤入 scrollback，收缩后只剩一屏空白。
            // 这里的提交标记仍在外层事务内，任何终端操作失败都会回滚。
            prepare_insert(terminal, app)?;
            let display = Arc::make_mut(&mut self.display_rows);
            for (index, line) in lines.iter().enumerate() {
                let tool_id = tool_ranges
                    .iter()
                    .find(|(range, _)| range.contains(&index))
                    .map(|(_, id)| id.clone());
                display.extend(
                    wrapped_history_rows(vec![line.clone()], width)
                        .into_iter()
                        .map(|spans| HistoryDisplayRow {
                            line: unpadded_history_line(spans),
                            tool_id: tool_id.clone(),
                        }),
                );
            }
            // 仅缓存一屏，缩放不会复制整段会话；更早正文由终端 scrollback 和持久事件保管。
            retain_screen_rows(display, terminal.size()?.height);
            insert_history_lines(terminal, lines, width)?;
            self.header_emitted = true;
        }
        Ok(changed || history_cleared)
    }

    fn stream_prefix_revised(&self, entry: &RenderedHistoryEntry, id: &EventId) -> bool {
        self.committed_stream_sources.get(id).map_or_else(
            || {
                self.committed_stream_prefixes
                    .get(id)
                    .is_some_and(|prefix| !entry.lines.starts_with(prefix))
            },
            |prefix| {
                entry
                    .source_prefix
                    .as_ref()
                    .is_some_and(|source| !source.starts_with(prefix))
            },
        )
    }

    fn append_event_lines(
        &mut self,
        app: &TuiApp,
        entries: Vec<RenderedHistoryEntry>,
        width: u16,
        lines: &mut Vec<Line<'static>>,
        tool_ranges: &mut Vec<(std::ops::Range<usize>, OperationId)>,
    ) {
        for entry in self
            .local_entries
            .iter_mut()
            .filter(|entry| entry.anchor.is_none() && !entry.emitted)
        {
            self.tail
                .append(lines, &local_entry_lines(app, &entry.items, width), None);
            entry.emitted = true;
        }
        for entry in entries {
            let event_id = entry.event_ids.first().copied();
            if entry.commit_event && entry.is_committed(&self.committed_event_ids) {
                continue;
            }
            let already = entry
                .event_ids
                .first()
                .and_then(|id| self.committed_stream_lines.get(id).copied())
                .unwrap_or(0);
            let commit_from = if entry.commit_event {
                already.min(entry.lines.len())
            } else {
                already.min(entry.stable_line_count)
            };
            let commit_until = if entry.commit_event {
                entry.lines.len()
            } else {
                entry.stable_line_count
            };
            if commit_until > commit_from {
                let mut cursor = commit_from;
                for local in self.local_entries.iter_mut().filter(|local| {
                    !local.emitted && local.anchor.is_some_and(|id| entry.event_ids.contains(&id))
                }) {
                    let offset = local.offset_in(app, &entry, width);
                    if offset > commit_until {
                        continue;
                    }
                    let offset = offset.max(cursor);
                    append_tool_fragment(
                        &mut self.tail,
                        lines,
                        &entry.lines[cursor..offset],
                        event_id,
                        entry.tool_id.as_ref(),
                        tool_ranges,
                    );
                    self.tail
                        .append(lines, &local_entry_lines(app, &local.items, width), None);
                    cursor = offset;
                    local.emitted = true;
                }
                append_tool_fragment(
                    &mut self.tail,
                    lines,
                    &entry.lines[cursor..commit_until],
                    event_id,
                    entry.tool_id.as_ref(),
                    tool_ranges,
                );
                self.last_anchor = entry.event_ids.last().copied();
                self.last_source_prefix = entry.source_prefix.clone();
            }
            if entry.commit_event {
                for event_id in &entry.event_ids {
                    self.committed_event_ids.insert(*event_id);
                    self.committed_stream_lines.remove(event_id);
                    self.committed_stream_prefixes.remove(event_id);
                    self.committed_stream_sources.remove(event_id);
                }
            } else if commit_until > already
                && let Some(event_id) = entry.event_ids.first().copied()
            {
                self.committed_stream_lines.insert(event_id, commit_until);
                self.committed_stream_prefixes
                    .insert(event_id, Arc::from(&entry.lines[..commit_until]));
                if let Some(source) = &entry.source_prefix {
                    self.committed_stream_sources
                        .insert(event_id, source.clone());
                }
            }
            // 后完成的工具不得越过尚未完成的前一个单元进入不可变历史。
            if !entry.commit_event {
                break;
            }
        }
    }

    fn append_local_messages(
        &mut self,
        app: &mut TuiApp,
        width: u16,
        lines: &mut Vec<Line<'static>>,
    ) {
        if !app.command_messages.is_empty() {
            // slash 命令没有 RuntimeEvent。推进 scrollback 后从 live 区拿掉，避免每次进出 overlay 再画一遍。
            let archived_lines = local_entry_lines(app, &app.command_messages, width);
            if !archived_lines.is_empty() {
                self.tail.append(lines, &archived_lines, None);
            }
            self.local_entries.push(LocalHistoryEntry {
                anchor: self.last_anchor,
                source_prefix: self.last_source_prefix.clone(),
                items: std::mem::take(&mut app.command_messages),
                emitted: true,
            });
            app.invalidate_transcript_layout();
        }
    }
}

fn local_entry_lines(app: &TuiApp, items: &[TranscriptItem], width: u16) -> Vec<Line<'static>> {
    let projections = items
        .iter()
        .cloned()
        .map(|item| {
            if matches!(
                item.role,
                TranscriptRole::User | TranscriptRole::Assistant | TranscriptRole::CommandResult
            ) {
                message_projection(item)
            } else {
                notice_projection(item)
            }
        })
        .collect();
    render_operation_projection_lines(app, projections, width)
}

fn rendered_history_entries(
    app: &TuiApp,
    width: u16,
    mode: InlineHistoryMode,
) -> Vec<RenderedHistoryEntry> {
    match mode {
        InlineHistoryMode::Transcript => {
            let entries = history_event_operations(app);
            entries
                .into_iter()
                .map(|entry| {
                    history_entry_from_projection(
                        app,
                        entry.event_ids,
                        entry.projection,
                        entry.stable,
                        width,
                    )
                })
                .collect()
        }
        InlineHistoryMode::Developer { expanded } => {
            let mut events = app.events.iter().collect::<Vec<_>>();
            events.sort_by_key(|event| event.sequence_no);
            developer_event_projections(events)
                .into_iter()
                .map(|event| {
                    let lines =
                        developer_event_history_lines(&event, width, expanded, app.palette());
                    let commit_event = !event.is_open_provider_stream();
                    RenderedHistoryEntry {
                        tool_id: None,
                        event_ids: event.event_ids,
                        stable_line_count: if commit_event {
                            lines.len()
                        } else {
                            lines.len().saturating_sub(1)
                        },
                        commit_event,
                        lines,
                        source_prefix: None,
                    }
                })
                .collect()
        }
        InlineHistoryMode::DebugSplit { expanded } => {
            debug_split_history_entries(app, width, expanded)
        }
    }
}

fn history_entry_from_projection(
    app: &TuiApp,
    event_ids: Vec<EventId>,
    projection: OperationProjection,
    stable: bool,
    width: u16,
) -> RenderedHistoryEntry {
    let tool_id = projection.id().cloned();
    let source_prefix = projection.is_assistant_message().then(|| {
        let source = projection.item(false).body.join("\n");
        if stable {
            source
        } else {
            source[..super::stream_commit::stable_source_end(&source)].to_owned()
        }
    });
    let stable_source = source_prefix
        .as_ref()
        .filter(|prefix| !prefix.is_empty())
        .map(|prefix| {
            let mut item = projection.item(false);
            item.body = vec![prefix.clone()];
            render_operation_projection_lines(app, vec![message_projection(item)], width)
        });
    let lines = render_operation_projection_lines(app, vec![projection], width);
    RenderedHistoryEntry {
        tool_id,
        event_ids,
        stable_line_count: if stable {
            lines.len()
        } else {
            stable_source.as_ref().map_or(0, |prefix| {
                super::stream_commit::stable_rendered_prefix(&lines, prefix)
            })
        },
        commit_event: stable,
        lines,
        source_prefix,
    }
}

fn debug_split_history_entries(
    app: &TuiApp,
    width: u16,
    expanded: bool,
) -> Vec<RenderedHistoryEntry> {
    let mut events = app.events.iter().collect::<Vec<_>>();
    events.sort_by_key(|event| event.sequence_no);
    debug_split_event_entries(app, events, width, expanded)
}

fn debug_source_events(app: &TuiApp) -> Vec<&golutra_agent_protocol::RuntimeEvent> {
    let mut events = if app.events.is_empty() {
        app.developer_projection
            .as_ref()
            .map(|projection| projection.events.iter().collect::<Vec<_>>())
            .unwrap_or_default()
    } else {
        app.events.iter().collect::<Vec<_>>()
    };
    events.sort_by_key(|event| event.sequence_no);
    events
}

fn debug_split_event_entries(
    app: &TuiApp,
    events: Vec<&golutra_agent_protocol::RuntimeEvent>,
    width: u16,
    expanded: bool,
) -> Vec<RenderedHistoryEntry> {
    let mut operations = event_operation_entries(&app.events)
        .into_iter()
        .flat_map(|entry| {
            let value = (entry.projection, entry.stable);
            entry
                .event_ids
                .into_iter()
                .map(move |event_id| (event_id, value.clone()))
        })
        .collect::<HashMap<_, _>>();
    let (transcript_width, developer_width) = debug_pane_widths(width);
    let mut blocked = false;
    let mut entries = Vec::new();
    for event in developer_event_projections(events) {
        let open_or_unstable = event.is_open_provider_stream()
            || event
                .event_ids
                .iter()
                .any(|event_id| operations.get(event_id).is_some_and(|(_, stable)| !*stable));
        if open_or_unstable {
            blocked = true;
        }
        let commit_event = !blocked;
        let transcript = event
            .event_ids
            .iter()
            .find_map(|event_id| operations.remove(event_id))
            .map(|(projection, _)| {
                render_operation_projection_lines(app, vec![projection], transcript_width)
            })
            .unwrap_or_default();
        let developer =
            developer_event_history_lines(&event, developer_width, expanded, app.palette());
        let lines = debug_split_history_lines(transcript, developer, width);
        entries.push(RenderedHistoryEntry {
            tool_id: None,
            event_ids: event.event_ids,
            stable_line_count: if commit_event {
                lines.len()
            } else {
                lines.len().saturating_sub(1)
            },
            commit_event,
            lines,
            source_prefix: None,
        });
    }
    entries
}

pub(crate) fn debug_split_live_lines(
    app: &TuiApp,
    width: u16,
    visible_rows: u16,
) -> Vec<Line<'static>> {
    let (transcript_width, developer_width) = debug_pane_widths(width);
    let facts = if !app.transcript.history.enabled || app.developer_error.is_some() {
        let mut facts = developer_fact_history_lines(app, developer_width);
        if facts.is_empty() && app.developer_projection.is_none() {
            facts.push(Line::from("loading developer projection"));
        }
        debug_split_history_lines(Vec::new(), facts, width)
    } else {
        Vec::new()
    };

    let mut timeline = debug_split_event_entries(
        app,
        debug_source_events(app),
        width,
        app.developer_observations_expanded,
    )
    .into_iter()
    .filter(|entry| !entry.is_committed(&app.transcript.history.committed_event_ids))
    .flat_map(|entry| entry.lines)
    .collect::<Vec<_>>();

    let live_event_operation_count = event_operation_entries(&app.events)
        .into_iter()
        .filter(|entry| {
            !entry.event_ids.iter().any(|event_id| {
                app.transcript
                    .history
                    .committed_event_ids
                    .contains(event_id)
            })
        })
        .count();
    let transcript_only = rendered_transcript_operation_projections(app)
        .into_iter()
        .skip(live_event_operation_count)
        .collect::<Vec<_>>();
    timeline.extend(debug_split_history_lines(
        render_operation_projection_lines(app, transcript_only, transcript_width),
        Vec::new(),
        width,
    ));

    if app.transcript.fullscreen {
        let mut lines = facts;
        lines.extend(timeline);
        return lines;
    }
    let capacity = usize::from(visible_rows);
    // Interactive history can rely on native scrollback. Offscreen snapshots cannot, so reserve
    // space for governance facts and prioritize user-visible transcript plus event headers.
    let fact_budget = if timeline.is_empty() {
        capacity
    } else {
        (capacity / 3).max(1)
    };
    let fact_count = facts.len().min(fact_budget);
    let timeline_count = timeline.len().min(capacity.saturating_sub(fact_count));
    let visible_timeline = if !app.transcript.history.enabled && timeline.len() > timeline_count {
        prioritized_debug_snapshot_lines(timeline, timeline_count, debug_pane_widths(width).0)
    } else {
        timeline
            .drain(timeline.len().saturating_sub(timeline_count)..)
            .collect::<Vec<_>>()
    };
    let gap = capacity.saturating_sub(fact_count + timeline_count);
    let mut lines = facts.into_iter().take(fact_count).collect::<Vec<_>>();
    lines.extend(std::iter::repeat_with(|| Line::from(" ".repeat(usize::from(width)))).take(gap));
    lines.extend(visible_timeline);
    lines
}

fn prioritized_debug_snapshot_lines(
    timeline: Vec<Line<'static>>,
    capacity: usize,
    transcript_width: u16,
) -> Vec<Line<'static>> {
    if timeline.len() <= capacity {
        return timeline;
    }

    let transcript_rows = timeline
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            debug_line_has_content_in_range(line, 0, transcript_width).then_some(index)
        })
        .collect::<Vec<_>>();
    let observation_rows = timeline
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            debug_line_has_content_in_range(line, transcript_width, u16::MAX).then_some(index)
        })
        .collect::<Vec<_>>();
    let observation_headers = observation_rows
        .iter()
        .copied()
        .filter(|index| {
            timeline[*index]
                .spans
                .iter()
                .any(|span| span.content.trim_start().starts_with('#'))
        })
        .collect::<Vec<_>>();
    let transcript_budget = capacity.saturating_sub(usize::from(!observation_rows.is_empty()));
    let mut selected = HashSet::with_capacity(capacity);
    if transcript_rows.len() <= transcript_budget {
        selected.extend(transcript_rows);
    } else {
        let head_count = transcript_budget / 2;
        let tail_count = transcript_budget.saturating_sub(head_count);
        selected.extend(transcript_rows.iter().take(head_count).copied());
        selected.extend(transcript_rows.iter().rev().take(tail_count).copied());
    }
    for index in observation_headers
        .into_iter()
        .rev()
        .chain(observation_rows.into_iter().rev())
    {
        if selected.len() >= capacity {
            break;
        }
        selected.insert(index);
    }
    for index in (0..timeline.len()).rev() {
        if selected.len() >= capacity {
            break;
        }
        selected.insert(index);
    }

    timeline
        .into_iter()
        .enumerate()
        .filter_map(|(index, line)| selected.contains(&index).then_some(line))
        .collect()
}

fn debug_line_has_content_in_range(line: &Line<'_>, start: u16, end: u16) -> bool {
    let mut column = 0_u16;
    for span in &line.spans {
        for grapheme in span.content.graphemes(true) {
            let grapheme_width = u16::try_from(display_width(grapheme)).unwrap_or(u16::MAX);
            let grapheme_end = column.saturating_add(grapheme_width);
            if column < end && grapheme_end > start && !grapheme.chars().all(char::is_whitespace) {
                return true;
            }
            column = grapheme_end;
            if column >= end {
                return false;
            }
        }
    }
    false
}

pub(crate) fn debug_split_history_lines(
    transcript: Vec<Line<'static>>,
    developer: Vec<Line<'static>>,
    width: u16,
) -> Vec<Line<'static>> {
    if width == 0 || (transcript.is_empty() && developer.is_empty()) {
        return Vec::new();
    }
    // A one-column terminal cannot represent either half of a strict split. Keep the degraded
    // state explicit instead of silently dropping both projections.
    if width == 1 {
        return vec![Line::from(HISTORY_OMISSION_MARKER)];
    }
    let (transcript_width, developer_width) = debug_pane_widths(width);
    let transcript_rows = wrapped_history_rows(transcript, transcript_width);
    let developer_rows = wrapped_history_rows(developer, developer_width);
    let row_count = transcript_rows.len().max(developer_rows.len());
    let mut rows = Vec::with_capacity(row_count);

    // Keep the two projections on the same physical rows. This makes an observation legible as
    // the counterpart of the transcript event that produced it, while still allowing either
    // side to continue with blank rows after the other side has finished wrapping.
    for index in 0..row_count {
        let mut spans = Vec::new();
        append_debug_pane_row(&mut spans, transcript_rows.get(index), transcript_width);
        append_debug_pane_row(&mut spans, developer_rows.get(index), developer_width);
        rows.push(Line::from(spans));
    }
    rows
}

fn append_debug_pane_row(
    destination: &mut Vec<Span<'static>>,
    row: Option<&Vec<Span<'static>>>,
    width: u16,
) {
    let used = row
        .map(|spans| spans.iter().map(Span::width).sum::<usize>())
        .unwrap_or_default();
    if let Some(spans) = row {
        destination.extend(spans.iter().cloned());
    }
    let padding = usize::from(width).saturating_sub(used);
    if padding > 0 {
        destination.push(Span::raw(" ".repeat(padding)));
    }
}

const RATATUI_MAX_BUFFER_CELLS: usize = u16::MAX as usize;
const RATATUI_MAX_SCROLL_ROWS: usize = u16::MAX as usize - 1;

fn max_history_chunk_rows(width: u16) -> usize {
    let width = usize::from(width.max(1));
    (RATATUI_MAX_BUFFER_CELLS / width).clamp(1, RATATUI_MAX_SCROLL_ROWS)
}

pub(crate) fn wrapped_history_rows(
    lines: Vec<Line<'static>>,
    width: u16,
) -> Vec<Vec<Span<'static>>> {
    if lines.is_empty() || width == 0 {
        return Vec::new();
    }
    // Ratatui 0.28 indexes Buffer rectangles with u16 arithmetic. Render pane rows in bounded
    // chunks, and segment extreme logical lines so neither area nor scroll offset can overflow.
    let max_chunk_rows = max_history_chunk_rows(width);
    let mut rows = Vec::new();

    for line in lines
        .into_iter()
        .flat_map(|line| bounded_history_line_segments(line, width))
    {
        let row_count = Paragraph::new(line.clone())
            .wrap(Wrap { trim: false })
            .line_count(width)
            .max(1);
        let mut offset = 0_usize;
        while offset < row_count {
            let chunk_rows = (row_count - offset).min(max_chunk_rows);
            let height = u16::try_from(chunk_rows).expect("debug history chunk is u16-bounded");
            let area = Rect::new(0, 0, width, height);
            let mut buffer = Buffer::empty(area);
            // Paragraph 的行样式不填满尾部空格；归档/缩放时显式保留整行 diff 底色。
            buffer.set_style(area, line.style);
            Paragraph::new(line.clone())
                .wrap(Wrap { trim: false })
                .scroll((ratatui_vertical_scroll(offset, height), 0))
                .render(area, &mut buffer);
            rows.extend((0..height).map(|row| styled_buffer_row(&buffer, row, width)));
            offset = offset.saturating_add(chunk_rows);
        }
    }

    rows
}

fn bounded_history_line_segments(line: Line<'static>, width: u16) -> Vec<Line<'static>> {
    let line_style = line.style;
    let line_alignment = line.alignment;
    let mut logical_lines = vec![Line {
        spans: Vec::new(),
        style: line_style,
        alignment: line_alignment,
    }];

    // A provider can put literal newlines inside one styled span. Ratatui treats those as
    // physical rows, so split them before applying the cell/grapheme bound below.
    for span in line.spans {
        let mut parts = span.content.split('\n').peekable();
        while let Some(part) = parts.next() {
            if !part.is_empty() {
                append_history_span(
                    &mut logical_lines
                        .last_mut()
                        .expect("history line accumulator is non-empty")
                        .spans,
                    part,
                    span.style,
                );
            }
            if parts.peek().is_some() {
                logical_lines.push(Line {
                    spans: Vec::new(),
                    style: line_style,
                    alignment: line_alignment,
                });
            }
        }
    }

    logical_lines
        .into_iter()
        .flat_map(|line| bounded_history_cell_segments(line, width))
        .collect()
}

fn append_history_span(spans: &mut Vec<Span<'static>>, content: &str, style: Style) {
    if let Some(previous) = spans.last_mut()
        && previous.style == style
    {
        previous.content.to_mut().push_str(content);
    } else {
        spans.push(Span::styled(content.to_owned(), style));
    }
}

fn bounded_history_cell_segments(line: Line<'static>, width: u16) -> Vec<Line<'static>> {
    let width = width.max(1);
    let width_cells = usize::from(width);
    // Emit physical rows instead of one giant logical line. Ratatui's word wrapper can turn a
    // cell-bounded logical line into many rows when words do not fit together; truncating that
    // line afterward loses its prefix. Hard-splitting here keeps every grapheme and lets the
    // normal paragraph renderer consume one already-bounded row at a time.
    let max_graphemes = max_history_chunk_rows(width)
        .saturating_mul(width_cells)
        .clamp(1, RATATUI_MAX_SCROLL_ROWS);
    let mut segments = Vec::new();
    let mut spans = Vec::<Span<'static>>::new();
    let mut grapheme_count = 0_usize;
    let mut cell_count = 0_usize;
    for span in line.spans {
        for grapheme in span.content.graphemes(true) {
            let raw_width = display_width(grapheme);
            // Ratatui intentionally ignores a grapheme wider than the target pane. Replace it
            // with a visible marker so narrow debug panes have an explicit degradation.
            let (grapheme, grapheme_width) = if raw_width > usize::from(width.max(1)) {
                (HISTORY_OMISSION_MARKER, 1)
            } else {
                (grapheme, raw_width)
            };
            if !spans.is_empty()
                && (cell_count.saturating_add(grapheme_width) > width_cells
                    || grapheme_count >= max_graphemes)
            {
                segments.push(Line {
                    spans: std::mem::take(&mut spans),
                    style: line.style,
                    alignment: line.alignment,
                });
                grapheme_count = 0;
                cell_count = 0;
            }
            append_history_span(&mut spans, grapheme, span.style);
            grapheme_count = grapheme_count.saturating_add(1);
            cell_count = cell_count.saturating_add(grapheme_width);
        }
    }
    if !spans.is_empty() || segments.is_empty() {
        segments.push(Line {
            spans,
            style: line.style,
            alignment: line.alignment,
        });
    }
    segments
}

fn styled_buffer_row(buffer: &Buffer, row: u16, width: u16) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut current_style = None;
    let mut current_text = String::new();
    let mut column = 0_u16;

    while column < width {
        let Some(cell) = buffer.cell((column, row)) else {
            break;
        };
        let style = cell.style();
        if current_style.is_some_and(|current| current != style) {
            spans.push(Span::styled(
                std::mem::take(&mut current_text),
                current_style.expect("debug history row has an active style"),
            ));
        }
        current_style = Some(style);
        let symbol_width = display_width(cell.symbol());
        if symbol_width == 0 {
            current_text.push(' ');
            column = column.saturating_add(1);
        } else {
            current_text.push_str(cell.symbol());
            column = column.saturating_add(u16::try_from(symbol_width).unwrap_or(u16::MAX).max(1));
        }
    }
    if !current_text.is_empty() {
        spans.push(Span::styled(
            current_text,
            current_style.expect("debug history row has an active style"),
        ));
    }
    spans
}

#[cfg(test)]
fn clear_history_terminal<B: Backend>(terminal: &mut Terminal<B>) -> io::Result<()> {
    let size = terminal.size()?;
    terminal.set_cursor_position(ratatui::layout::Position::ORIGIN)?;
    terminal
        .backend_mut()
        .clear_region(ratatui::backend::ClearType::All)?;
    terminal.resize(ratatui::layout::Rect::new(0, 0, size.width, size.height))?;
    terminal.current_buffer_mut().reset();
    Ok(())
}

pub(crate) fn inline_viewport_height(app: &TuiApp, width: u16, screen_height: u16) -> u16 {
    // 候选属于活动输入区，必须计入高度；通过原生视口扩缩腾出空间，不覆盖历史或切入备用屏。
    let bottom = bottom_pane_height_for_width(app, width).max(MIN_INLINE_BOTTOM_ROWS);
    let live = live_transcript_body_rows(app, width);
    bottom.saturating_add(live).min(screen_height).max(1)
}

fn live_transcript_body_rows(app: &TuiApp, width: u16) -> u16 {
    // 高度必须按 live 渲染行计算，已经推进 scrollback 的流式前缀不能再把 composer 撑高。
    let lines = live_transcript_render_rows(app, width)
        .into_iter()
        .map(|row| row.line)
        .collect::<Vec<_>>();
    if lines.is_empty() {
        return 0;
    }
    u16::try_from(history_lines_height(&lines, width)).unwrap_or(u16::MAX)
}

pub(crate) fn sync_inline_viewport_height(
    terminal: &mut InteractiveTerminal,
    app: &TuiApp,
) -> io::Result<()> {
    let size = terminal.size()?;
    let desired = inline_viewport_height(app, size.width.max(1), size.height.max(1));
    let current = terminal.current_buffer_mut().area;
    if current.height == desired && current.width == size.width && current.bottom() <= size.height {
        return Ok(());
    }
    terminal.resize_inline(desired, size)
}

fn insert_history_lines<B: Backend>(
    terminal: &mut Terminal<B>,
    lines: Vec<Line<'static>>,
    width: u16,
) -> io::Result<()> {
    let width = width.max(1);
    // Ratatui 0.28 clamps Buffer area to u16::MAX while indexing the full rectangle.
    let max_rows = max_history_chunk_rows(width);
    let mut batch = Vec::new();
    let mut batch_rows = 0_usize;

    for mut line in lines
        .into_iter()
        .flat_map(|line| bounded_history_line_segments(line, width))
    {
        if line
            .style
            .bg
            .is_some_and(|color| color != ratatui::style::Color::Reset)
        {
            // 历史缓存只存文字；按当前物理宽度重新补色，不能复用旧窗口的填充量。
            let padding = usize::from(width).saturating_sub(line.width());
            line.spans
                .push(Span::styled(" ".repeat(padding), line.style));
        }
        let line_rows = history_line_height(&line, width);
        if line_rows > max_rows {
            insert_history_batch(terminal, std::mem::take(&mut batch), batch_rows)?;
            batch_rows = 0;
            insert_tall_history_line(terminal, line, line_rows, max_rows)?;
            continue;
        }
        if batch_rows.saturating_add(line_rows) > max_rows {
            insert_history_batch(terminal, std::mem::take(&mut batch), batch_rows)?;
            batch_rows = 0;
        }
        batch_rows = batch_rows.saturating_add(line_rows);
        batch.push(line);
    }
    insert_history_batch(terminal, batch, batch_rows)
}

fn insert_history_batch<B: Backend>(
    terminal: &mut Terminal<B>,
    lines: Vec<Line<'static>>,
    rows: usize,
) -> io::Result<()> {
    if lines.is_empty() || rows == 0 {
        return Ok(());
    }
    let height = u16::try_from(rows)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "history batch is too tall"))?;
    terminal.insert_before(height, move |buffer| {
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(buffer.area, buffer);
    })
}

fn insert_tall_history_line<B: Backend>(
    terminal: &mut Terminal<B>,
    line: Line<'static>,
    rows: usize,
    max_rows: usize,
) -> io::Result<()> {
    if rows > RATATUI_MAX_SCROLL_ROWS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "one history line exceeds the terminal scroll limit",
        ));
    }
    let mut offset = 0_usize;
    while offset < rows {
        let chunk_rows = (rows - offset).min(max_rows);
        let height = u16::try_from(chunk_rows)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "history chunk is too tall"))?;
        let line = line.clone();
        terminal.insert_before(height, move |buffer| {
            Paragraph::new(line)
                .wrap(Wrap { trim: false })
                .scroll((ratatui_vertical_scroll(offset, height), 0))
                .render(buffer.area, buffer);
        })?;
        offset = offset.saturating_add(chunk_rows);
    }
    Ok(())
}

fn history_line_height(line: &Line<'static>, width: u16) -> usize {
    Paragraph::new(line.clone())
        .wrap(Wrap { trim: false })
        .line_count(width.max(1))
        .max(1)
}

fn history_lines_height(lines: &[Line<'static>], width: u16) -> usize {
    lines
        .iter()
        .map(|line| history_line_height(line, width))
        .sum()
}

#[cfg(test)]
mod boundary_tests {
    use super::*;

    #[test]
    fn growing_stream_table_and_terminal_resize_do_not_duplicate_the_final_answer() {
        let session_id = SessionId::new();
        let turn_id = TurnId::new();
        let mut app = TuiApp::new(
            ThreadId::new(),
            session_id,
            None,
            false,
            "mock".into(),
            None,
        );
        app.enable_inline_history();
        let first = "后台任务已启动，接下来展示各个终端的运行结果。\n\n| Target | Result |\n| --- | --- |\n| one | ok |\n";
        let second =
            "| two | a much longer result that changes the column width |\n\n任务全部完成。";
        let mut event = RuntimeEvent {
            schema_version: golutra_agent_core::RUNTIME_EVENT_SCHEMA_VERSION,
            causal_context: Default::default(),
            causal_links: vec![],
            id: EventId::new(),
            sequence_no: 1,
            session_id,
            turn_id: Some(turn_id),
            task_id: None,
            parent_event_id: None,
            event_type: golutra_agent_protocol::RuntimeEventType::ProviderStreamed,
            timestamp: chrono::Utc::now(),
            source: golutra_agent_protocol::RuntimeEventSource::Provider,
            payload: json!({"delta":{"kind":"text_delta","text":first}}),
            payload_ref: None,
            durable: false,
        };
        app.events.push(event.clone());
        let mut terminal = Terminal::with_options(
            ratatui::backend::TestBackend::new(100, 120),
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Inline(3),
            },
        )
        .unwrap();
        let mut history = InlineHistoryState::new(session_id);
        history.native_scrollback = true;
        history
            .flush_with_rebuild(&mut terminal, &mut app, |_| {
                panic!("must preserve scrollback")
            })
            .unwrap();
        terminal.backend_mut().resize(60, 120);
        terminal.autoresize().unwrap();
        event.id = EventId::new();
        event.sequence_no = 2;
        event.payload = json!({"delta":{"kind":"text_delta","text":second}});
        app.events.push(event.clone());
        history
            .flush_with_rebuild(&mut terminal, &mut app, |_| {
                panic!("must preserve scrollback")
            })
            .unwrap();
        event.id = EventId::new();
        event.sequence_no = 3;
        event.event_type = golutra_agent_protocol::RuntimeEventType::AssistantMessage;
        event.payload = json!({"content":format!("{first}{second}")});
        app.events.push(event);
        history
            .flush_with_rebuild(&mut terminal, &mut app, |_| {
                panic!("must preserve scrollback")
            })
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!text.contains("Updated response"), "{text}");
        let compact = text
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();
        assert!(compact.contains("任务全部完成"), "{text}");
    }

    #[test]
    fn native_history_preserves_commits_and_labels_a_revised_final_without_clearing() {
        let session_id = SessionId::new();
        let turn_id = TurnId::new();
        let mut app = TuiApp::new(
            ThreadId::new(),
            session_id,
            None,
            false,
            "mock".into(),
            None,
        );
        app.enable_inline_history();
        let mut event = RuntimeEvent {
            schema_version: golutra_agent_core::RUNTIME_EVENT_SCHEMA_VERSION,
            causal_context: Default::default(),
            causal_links: vec![],
            id: EventId::new(),
            sequence_no: 1,
            session_id,
            turn_id: Some(turn_id),
            task_id: None,
            parent_event_id: None,
            event_type: golutra_agent_protocol::RuntimeEventType::ProviderStreamed,
            timestamp: chrono::Utc::now(),
            source: golutra_agent_protocol::RuntimeEventSource::Tool,
            payload: json!({"delta":{"kind":"text_delta","text":"Earlier paragraph.\n\nUnfinished"}}),
            payload_ref: None,
            durable: false,
        };
        app.events.push(event.clone());
        let mut terminal = Terminal::with_options(
            ratatui::backend::TestBackend::new(100, 80),
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Inline(3),
            },
        )
        .unwrap();
        let mut history = InlineHistoryState::new(session_id);
        history.native_scrollback = true;
        history
            .flush_with_rebuild(&mut terminal, &mut app, |_| {
                panic!("native history must not clear terminal")
            })
            .unwrap();
        assert!(!history.committed_stream_prefixes.is_empty());
        app.request_history_rebuild();
        event.id = EventId::new();
        event.sequence_no = 2;
        event.event_type = golutra_agent_protocol::RuntimeEventType::AssistantMessage;
        event.payload = json!({"content":"Corrected final answer."});
        app.events.push(event);
        history
            .flush_with_rebuild(&mut terminal, &mut app, |_| {
                panic!("native history must not clear terminal")
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
            text.contains("Earlier paragraph."),
            "already emitted facts stay in native history"
        );
        let notice = text.find("Updated response").expect("revision marker");
        assert!(
            notice
                < text
                    .find("Corrected final answer.")
                    .expect("complete final answer")
        );
        assert!(
            !history
                .flush_with_rebuild(&mut terminal, &mut app, |_| panic!("unexpected reset"))
                .unwrap()
        );
    }

    #[test]
    fn debug_history_does_not_truncate_beyond_the_ratatui_buffer_boundary() {
        let pane_width = 1_u16;
        let expected_rows = usize::from(u16::MAX) + 1;
        let content = "x".repeat(expected_rows * usize::from(pane_width));
        let rows = wrapped_history_rows(vec![Line::from(content)], pane_width);

        assert_eq!(rows.len(), expected_rows);
        assert_eq!(
            rows.iter()
                .flat_map(|row| row.iter())
                .map(|span| span.content.matches('x').count())
                .sum::<usize>(),
            expected_rows * usize::from(pane_width)
        );
        assert!(rows.iter().all(|row| {
            row.iter().map(|span| span.width()).sum::<usize>() == usize::from(pane_width)
        }));
    }

    #[test]
    fn word_spaced_history_preserves_content_when_word_wrapping_would_exceed_the_row_cap() {
        let width = 80_u16;
        let unit = format!("{} ", "x".repeat(41));
        let content = format!("{}TAIL", unit.repeat(1_558));
        assert_eq!(content.len(), 65_440);
        let expected_x_count = content.matches('x').count();

        let rows = wrapped_history_rows(vec![Line::from(content)], width);
        assert!(rows.len() <= max_history_chunk_rows(width));
        let rendered = rows
            .iter()
            .flat_map(|row| row.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(!rendered.contains(HISTORY_OMISSION_MARKER));
        assert_eq!(rendered.matches('x').count(), expected_x_count);
        assert!(rendered.contains("TAIL"));
    }

    #[test]
    fn debug_history_splits_embedded_newlines_before_rendering() {
        let pane_width = 8_u16;
        let newline_count = 70_000;
        let content = "x\n".repeat(newline_count);
        let rows = wrapped_history_rows(vec![Line::from(vec![Span::raw(content)])], pane_width);

        assert_eq!(
            rows.iter()
                .flat_map(|row| row.iter())
                .map(|span| span.content.matches('x').count())
                .sum::<usize>(),
            newline_count
        );
        assert!(rows.len() >= newline_count);
        assert!(rows.iter().all(|row| {
            row.iter().map(|span| span.width()).sum::<usize>() == usize::from(pane_width)
        }));
    }

    #[test]
    fn debug_split_content_measurement_ignores_zero_width_joiners() {
        let line = Line::from("x\u{200d}");
        assert!(!debug_line_has_content_in_range(&line, 1, 2));
        assert!(debug_line_has_content_in_range(&line, 0, 1));

        let emoji = Line::from("👩\u{200d}💻");
        assert!(debug_line_has_content_in_range(&emoji, 0, 1));
        assert!(debug_line_has_content_in_range(&emoji, 1, 2));
    }
}
