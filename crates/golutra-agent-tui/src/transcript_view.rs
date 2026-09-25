//! Pure mapping from runtime/user projections to transcript view models.

use std::collections::{HashMap, HashSet};

use golutra_agent_core::{
    EventId, FileChangeKind, FileChangeSummary, TaskId, TaskStatus, ToolResultStatus, TurnId,
    UserStep, UserStepKind,
};
use golutra_agent_protocol::{RuntimeEvent, RuntimeEventType, UserProjection, VisibleStep};
use serde_json::Value;

#[path = "transcript_terminal.rs"]
mod terminal;

use super::{
    BodyViewMode, PaneScrollState, TranscriptLayoutCache, TranscriptPresentation,
    TranscriptSearchState, TuiApp, operation_file_changes,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TranscriptRole {
    User,
    Assistant,
    Status,
    Activity,
    Success,
    Warning,
    Error,
    System,
    // slash 命令结果画在 › /resume 下面，用 ⎿ 收口成功或取消。
    CommandResult,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TranscriptItem {
    pub(crate) role: TranscriptRole,
    pub(crate) title: String,
    pub(crate) body: Vec<String>,
}

/// 本地命令按事件和源文本锚定；流式续写、换行重排不能改变它的时间位置。
#[derive(Debug, Clone)]
pub(crate) struct LocalTranscriptEntry {
    anchor: Option<EventId>,
    source_prefix: Option<String>,
    items: Vec<TranscriptItem>,
}

pub(crate) fn archive_local_messages(app: &mut TuiApp) {
    if !app.transcript.fullscreen
        || app.transcript.history.enabled
        || app.command_messages.is_empty()
    {
        return;
    }
    let entries = history_event_operations(app);
    let last = entries.last();
    let anchor = last.and_then(|entry| entry.event_ids.last().copied());
    let source_prefix = last.and_then(|entry| match &entry.projection {
        OperationProjection::Message { item } if item.role == TranscriptRole::Assistant => {
            let mut source = item.body.join("\n");
            if !entry.stable {
                // 命令插在完整 Markdown 块之间，不能把正在生成的一句话或代码围栏切断。
                source.truncate(super::stream_commit::stable_source_end(&source));
            }
            Some(source)
        }
        _ => None,
    });
    if let Some(anchor) = anchor {
        app.transcript.history.command_anchors.insert(anchor);
    }
    app.transcript.local_entries.push(LocalTranscriptEntry {
        anchor,
        source_prefix,
        items: std::mem::take(&mut app.command_messages),
    });
}

fn interleave_local_entries(
    entries: Vec<EventOperationEntry>,
    locals: &[LocalTranscriptEntry],
) -> Vec<OperationProjection> {
    let mut result = Vec::new();
    // 被分页裁掉的锚点仍保留本地记录，在已加载历史的最前端显示。
    for local in locals.iter().filter(|local| {
        local
            .anchor
            .is_none_or(|id| !entries.iter().any(|entry| entry.event_ids.contains(&id)))
    }) {
        result.extend(local.items.iter().cloned().map(notice_projection));
    }
    for entry in entries {
        let attached = locals
            .iter()
            .filter(|local| local.anchor.is_some_and(|id| entry.event_ids.contains(&id)))
            .collect::<Vec<_>>();
        if attached.is_empty() {
            result.push(entry.projection);
            continue;
        }
        if let OperationProjection::Message { item } = &entry.projection
            && item.role == TranscriptRole::Assistant
        {
            let source = item.body.join("\n");
            let mut offset = 0;
            for local in attached {
                // final 若修订了流式前缀，保留完整 final，并把命令放在其后，绝不截掉修订文本。
                let end = local
                    .source_prefix
                    .as_ref()
                    .filter(|prefix| source.starts_with(prefix.as_str()))
                    .map_or(source.len(), String::len)
                    .max(offset);
                if end > offset {
                    let mut part = item.clone();
                    part.body = vec![source[offset..end].to_owned()];
                    result.push(message_projection(part));
                }
                offset = end;
                result.extend(local.items.iter().cloned().map(notice_projection));
            }
            if offset < source.len() {
                let mut part = item.clone();
                part.body = vec![source[offset..].to_owned()];
                result.push(message_projection(part));
            }
        } else {
            result.push(entry.projection);
            for local in attached {
                result.extend(local.items.iter().cloned().map(notice_projection));
            }
        }
    }
    result
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct OperationId(String);

impl OperationId {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TranscriptHistoryState {
    pub(crate) reflow_pending: bool,
    pub(crate) native_render_width: Option<u16>,
    pub(crate) enabled: bool,
    pub(crate) committed_event_ids: HashSet<EventId>,
    pub(crate) committed_stream_lines: HashMap<EventId, usize>,
    pub(crate) command_anchors: HashSet<EventId>,
    pub(crate) tail: super::transcript_spacing::TranscriptTail,
    pub(crate) replay_generation: u64,
    pub(crate) replay_ready: bool,
}

impl Default for TranscriptHistoryState {
    fn default() -> Self {
        Self {
            reflow_pending: false,
            native_render_width: None,
            enabled: false,
            committed_event_ids: HashSet::new(),
            committed_stream_lines: HashMap::new(),
            command_anchors: HashSet::new(),
            tail: super::transcript_spacing::TranscriptTail::default(),
            replay_generation: 0,
            replay_ready: true,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TranscriptState {
    pub(crate) frame_cache_enabled: bool,
    frame_operations: std::cell::RefCell<Option<HistoryProjectionCache>>,
    pub(crate) markdown_cache: std::cell::RefCell<super::rich_text::MarkdownCache>,
    pub(crate) frame_layout:
        std::cell::RefCell<Option<(u64, u16, super::transcript_widget::TranscriptLayout)>>,
    pub(crate) compact_tools: bool,
    pub(crate) fullscreen: bool,
    pub(crate) local_entries: Vec<LocalTranscriptEntry>,
    pub(crate) expanded_operations: HashSet<OperationId>,
    collapsed_operations: HashSet<OperationId>,
    pub(crate) details_expanded: bool,
    pub(crate) scroll: PaneScrollState,
    pub(crate) top_row_override: Option<usize>,
    pub(crate) revision: u64,
    pub(crate) layout_cache: Option<TranscriptLayoutCache>,
    pub(crate) history: TranscriptHistoryState,
    pub(crate) presentation: TranscriptPresentation,
    pub(crate) search: Option<TranscriptSearchState>,
    pub(crate) search_restore_body_view: Option<BodyViewMode>,
}

impl Default for TranscriptState {
    fn default() -> Self {
        Self {
            markdown_cache: Default::default(),
            frame_cache_enabled: false,
            frame_operations: Default::default(),
            frame_layout: Default::default(),
            expanded_operations: HashSet::new(),
            collapsed_operations: HashSet::new(),
            fullscreen: false,
            compact_tools: false,
            local_entries: Vec::new(),
            details_expanded: false,
            scroll: PaneScrollState {
                follow_tail: true,
                ..PaneScrollState::default()
            },
            top_row_override: None,
            revision: 0,
            layout_cache: None,
            history: TranscriptHistoryState::default(),
            presentation: TranscriptPresentation::Rich,
            search: None,
            search_restore_body_view: None,
        }
    }
}

impl TranscriptState {
    pub(crate) fn invalidate_layout(&mut self) {
        self.frame_operations.get_mut().take();
        self.invalidate_visual_layout();
    }

    pub(crate) fn invalidate_visual_layout(&mut self) {
        self.frame_layout.get_mut().take();
        self.revision = self.revision.wrapping_add(1);
        self.layout_cache = None;
        if let Some(cache) = self.frame_operations.get_mut() {
            cache.revision = self.revision;
        }
    }

    /// 非展示事件保留投影；同回合正文只追加尾部，语义边界和裁剪则完整重建。
    pub(crate) fn invalidate_for_event(&mut self, event: &RuntimeEvent, event_count: usize) {
        let mut cache = self.frame_operations.get_mut().take();
        self.invalidate_visual_layout();
        if let Some(cached) = cache.as_mut()
            && cached.event_count + 1 == event_count
            && cached.anchors == self.history.command_anchors
            && cached.advance(event)
        {
            cached.event_count = event_count;
            cached.revision = self.revision;
            *self.frame_operations.get_mut() = cache;
        }
    }

    pub(crate) fn reset_view(&mut self) {
        self.expanded_operations.clear();
        self.collapsed_operations.clear();
        self.details_expanded = false;
        self.top_row_override = None;
        self.invalidate_layout();
    }

    #[cfg(test)]
    pub(crate) fn toggle_operation(&mut self, id: OperationId) {
        let overrides = if self.details_expanded {
            &mut self.collapsed_operations
        } else {
            &mut self.expanded_operations
        };
        if !overrides.insert(id.clone()) {
            overrides.remove(&id);
        }
        self.invalidate_layout();
    }

    pub(crate) fn toggle_details(&mut self) -> bool {
        self.details_expanded = !self.details_expanded;
        self.expanded_operations.clear();
        self.collapsed_operations.clear();
        self.invalidate_layout();
        self.details_expanded
    }

    pub(crate) fn is_expanded(&self, id: Option<&OperationId>) -> bool {
        if self.details_expanded {
            !id.is_some_and(|id| self.collapsed_operations.contains(id))
        } else {
            id.is_some_and(|id| self.expanded_operations.contains(id))
        }
    }

    pub(crate) fn enable_inline_history(&mut self) {
        if !self.history.enabled {
            self.history.enabled = true;
            self.invalidate_layout();
        }
    }

    pub(crate) fn begin_history_replay(&mut self) {
        self.history.tail = super::transcript_spacing::TranscriptTail::default();
        self.history.replay_generation = self.history.replay_generation.wrapping_add(1);
        self.history.replay_ready = false;
        self.set_committed_event_ids(HashSet::new());
        self.set_committed_stream_lines(HashMap::new());
    }

    pub(crate) fn request_history_rebuild(&mut self) {
        self.history.tail = super::transcript_spacing::TranscriptTail::default();
        self.history.replay_generation = self.history.replay_generation.wrapping_add(1);
        self.set_committed_event_ids(HashSet::new());
        self.set_committed_stream_lines(HashMap::new());
    }

    pub(crate) fn set_committed_event_ids(&mut self, ids: HashSet<EventId>) {
        if self.history.committed_event_ids != ids {
            self.history.committed_event_ids = ids;
            self.history
                .committed_stream_lines
                .retain(|event_id, _| !self.history.committed_event_ids.contains(event_id));
            self.invalidate_layout();
        }
    }

    pub(crate) fn set_committed_stream_lines(&mut self, lines: HashMap<EventId, usize>) {
        if self.history.committed_stream_lines != lines {
            self.history.committed_stream_lines = lines;
            self.invalidate_visual_layout();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OperationProjection {
    Message {
        item: TranscriptItem,
    },
    ToolActivity {
        id: OperationId,
        item: TranscriptItem,
        details: Vec<String>,
    },
    FileChange {
        id: OperationId,
        item: TranscriptItem,
        details: Vec<String>,
    },
    Notice {
        item: TranscriptItem,
    },
}

impl OperationProjection {
    pub(crate) fn id(&self) -> Option<&OperationId> {
        match self {
            Self::ToolActivity { id, .. } | Self::FileChange { id, .. } => Some(id),
            Self::Message { .. } | Self::Notice { .. } => None,
        }
    }

    pub(crate) fn is_expandable(&self) -> bool {
        match self {
            Self::ToolActivity { details, .. } | Self::FileChange { details, .. } => {
                !details.is_empty()
            }
            Self::Message { .. } | Self::Notice { .. } => false,
        }
    }

    pub(crate) fn item(&self, expanded: bool) -> TranscriptItem {
        let (item, details) = match self {
            Self::Message { item } | Self::Notice { item } => return item.clone(),
            Self::ToolActivity { item, details, .. } | Self::FileChange { item, details, .. } => {
                (item, details)
            }
        };
        let mut item = item.clone();
        if matches!(self, Self::FileChange { .. }) && details.iter().any(|line| line == "Diff") {
            // 多文件的路径已在各自 diff 段落标识，不再重复打印前置文件清单。
            item.body.retain(|line| {
                !details.iter().any(|detail| {
                    ["added ", "modified ", "deleted "]
                        .iter()
                        .any(|prefix| detail.strip_prefix(prefix) == Some(line.as_str()))
                })
            });
        }
        let mut preview = if matches!(self, Self::FileChange { .. }) {
            let mut preview = crate::tool_preview::file_preview(details, expanded);
            if expanded && matches!(item.role, TranscriptRole::Error | TranscriptRole::Warning) {
                // 修改文件的失败命令仍需展示真实诊断，不能因精简 diff 吞掉失败输出。
                let end = details
                    .iter()
                    .position(|line| line == "Diff")
                    .unwrap_or(details.len());
                preview.splice(0..0, details[..end].iter().cloned());
            }
            preview
        } else if expanded {
            details.clone()
        } else {
            default_tool_preview(
                details,
                item.title == "ran"
                    || item.title.contains("shell")
                    || item.title.starts_with("Background terminal"),
                item.role == TranscriptRole::Activity,
            )
        };
        if !expanded && item.role == TranscriptRole::Activity {
            // 进行中的进程保留有界尾部；文件正文和参数不进入默认卡片。
            let output_start = details
                .iter()
                .position(|line| line == "Output")
                .unwrap_or(details.len());
            let mut tail = details[..output_start]
                .iter()
                .rev()
                .filter(|line| line.starts_with("│ "))
                .take(5)
                .cloned()
                .collect::<Vec<_>>();
            tail.reverse();
            preview.extend(tail);
        }
        item.body.extend(preview);
        item
    }

    fn item_mut(&mut self) -> &mut TranscriptItem {
        match self {
            Self::Message { item }
            | Self::ToolActivity { item, .. }
            | Self::FileChange { item, .. }
            | Self::Notice { item } => item,
        }
    }

    pub(crate) fn is_assistant_message(&self) -> bool {
        matches!(self, Self::Message { item } if item.role == TranscriptRole::Assistant)
    }

    fn is_completed_tool(&self) -> bool {
        match self {
            Self::ToolActivity { item, .. } | Self::FileChange { item, .. } => {
                !matches!(item.role, TranscriptRole::Activity)
            }
            Self::Message { .. } | Self::Notice { .. } => false,
        }
    }

    fn is_successful_completed_tool(&self) -> bool {
        self.is_completed_tool() && self.item(false).role == TranscriptRole::Success
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn transcript_items(app: &TuiApp) -> Vec<TranscriptItem> {
    transcript_operation_projections(app)
        .into_iter()
        .map(|projection| {
            let expanded = app.transcript.is_expanded(projection.id());
            projection.item(expanded)
        })
        .collect()
}

pub(crate) fn transcript_operation_projections(app: &TuiApp) -> Vec<OperationProjection> {
    transcript_operation_projections_after(app, None)
}

pub(crate) fn rendered_transcript_operation_projections(app: &TuiApp) -> Vec<OperationProjection> {
    let committed = (app.transcript.history.enabled
        && !app.transcript.fullscreen
        && app.transcript.search.is_none())
    .then_some(&app.transcript.history.committed_event_ids);
    transcript_operation_projections_after(app, committed)
}

fn transcript_operation_projections_after(
    app: &TuiApp,
    committed_event_ids: Option<&HashSet<EventId>>,
) -> Vec<OperationProjection> {
    if app.auth_dialog.is_some() {
        return Vec::new();
    }
    let mut items: Vec<OperationProjection> = Vec::new();
    let event_items = if committed_event_ids.is_some() || app.transcript.fullscreen {
        history_event_operations(app)
    } else {
        event_operation_entries(&app.events)
    };
    let has_event_items = !event_items.is_empty();
    let event_items = event_items
        .into_iter()
        .filter(|entry| {
            committed_event_ids.is_none_or(|committed| {
                !entry
                    .event_ids
                    .iter()
                    .all(|event_id| committed.contains(event_id))
            })
        })
        .collect::<Vec<_>>();
    items.extend(interleave_local_entries(
        event_items,
        &app.transcript.local_entries,
    ));
    items.extend(app.command_messages.iter().cloned().map(notice_projection));
    if let Some(projection) = &app.projection {
        // 主 transcript 只承载对话和执行过程；验证结论与残余风险由 /debug 面板统一展示。
        if !has_event_items {
            items.extend(
                projection_items(projection)
                    .into_iter()
                    .map(plain_projection),
            );
        }
    }
    items
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn event_transcript_items(events: &[RuntimeEvent]) -> Vec<TranscriptItem> {
    event_operation_projections(events)
        .into_iter()
        .map(|projection| projection.item(false))
        .collect()
}

pub(crate) fn event_operation_projections(events: &[RuntimeEvent]) -> Vec<OperationProjection> {
    event_operation_entries(events)
        .into_iter()
        .map(|entry| entry.projection)
        .collect()
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn stable_event_operation_projection_count(events: &[RuntimeEvent]) -> usize {
    event_operation_entries(events)
        .into_iter()
        .take_while(|entry| entry.stable)
        .count()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EventOperationEntry {
    process_id: Option<String>,
    terminal_wait: bool,
    tool_kind: ToolSummaryKind,
    pub(crate) id: EventId,
    pub(crate) event_ids: Vec<EventId>,
    pub(crate) projection: OperationProjection,
    pub(crate) stable: bool,
    task_id: Option<TaskId>,
    turn_id: Option<TurnId>,
}

impl EventOperationEntry {
    fn new(event: &RuntimeEvent, projection: OperationProjection, stable: bool) -> Self {
        let facts = event
            .payload
            .get("arguments")
            .or_else(|| event.payload.pointer("/envelope/structured_facts"))
            .unwrap_or(&event.payload);
        Self {
            process_id: facts
                .get("process_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            terminal_wait: event
                .payload
                .get("tool_name")
                .or_else(|| event.payload.pointer("/envelope/tool_name"))
                .and_then(Value::as_str)
                == Some("shell_session")
                && facts.get("action").and_then(Value::as_str) == Some("wait"),
            tool_kind: ToolSummaryKind::from_tool_name(
                event
                    .payload
                    .get("tool_name")
                    .or_else(|| {
                        event
                            .payload
                            .get("envelope")
                            .and_then(|value| value.get("tool_name"))
                    })
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            ),
            id: event.id,
            event_ids: vec![event.id],
            projection,
            task_id: event.task_id,
            turn_id: event.turn_id,
            stable,
        }
    }
}

#[derive(Debug, Default)]
struct ProjectionIndexes {
    visible_user_turns: HashMap<TurnId, usize>,
    pending_user_turns: HashMap<TurnId, TranscriptItem>,
    streamed_assistant_items: HashMap<TurnId, usize>,
    active_tools: HashMap<OperationId, usize>,
    recovering_requests: HashSet<(Option<TurnId>, Option<String>)>,
}

fn recovery_request_key(event: &RuntimeEvent) -> (Option<TurnId>, Option<String>) {
    (
        event.turn_id,
        event
            .payload
            .get("provider_request_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
    )
}

fn update_recovering_requests(
    requests: &mut HashSet<(Option<TurnId>, Option<String>)>,
    event: &RuntimeEvent,
) {
    if event.event_type == RuntimeEventType::RetryScheduled
        && event.payload["recovery"]["reset_stream"].as_bool() == Some(true)
    {
        requests.insert(recovery_request_key(event));
    } else if matches!(
        event.event_type,
        RuntimeEventType::ProviderCompleted | RuntimeEventType::ProviderFailed
    ) {
        requests.remove(&recovery_request_key(event));
    } else if event.event_type == RuntimeEventType::AssistantMessage
        || event.event_type.is_task_terminal()
    {
        requests.retain(|(turn, _)| *turn != event.turn_id);
    }
}

fn user_step_projection(step: &UserStep) -> Option<OperationProjection> {
    match &step.kind {
        UserStepKind::AssistantText { text } => {
            let text = text.trim();
            if text.is_empty() {
                return None;
            }
            Some(message_projection(TranscriptItem {
                role: TranscriptRole::Assistant,
                title: "Golutra".to_owned(),
                body: vec![text.to_owned()],
            }))
        }
        UserStepKind::ToolBatch { summary, tools } => {
            let failed = tools
                .iter()
                .filter(|tool| tool.status != ToolResultStatus::Ok)
                .collect::<Vec<_>>();
            let title = if summary.trim().is_empty() {
                "ran".to_owned()
            } else {
                summary.clone()
            };
            let details = tools
                .iter()
                .filter_map(|tool| tool.object.clone())
                .collect::<Vec<_>>();
            Some(OperationProjection::ToolActivity {
                id: OperationId(step.step_id.to_string()),
                item: TranscriptItem {
                    role: if failed.is_empty() {
                        TranscriptRole::Success
                    } else {
                        TranscriptRole::Error
                    },
                    title,
                    body: Vec::new(),
                },
                details,
            })
        }
    }
}

pub(crate) fn event_operation_entries(events: &[RuntimeEvent]) -> Vec<EventOperationEntry> {
    event_operation_entries_with_boundary(
        events,
        &HashSet::new(),
        &HashSet::new(),
        false,
        &HashSet::new(),
    )
}

/// 已写入终端的工具单元是不可变边界，后来的调用只能进入新的单元。
#[derive(Debug, Clone)]
struct HistoryProjectionCache {
    revision: u64,
    event_count: usize,
    anchors: HashSet<EventId>,
    entries: Vec<EventOperationEntry>,
    recovering_requests: HashSet<(Option<TurnId>, Option<String>)>,
}

impl HistoryProjectionCache {
    fn advance(&mut self, event: &RuntimeEvent) -> bool {
        if event.event_type == RuntimeEventType::ProviderStreamed
            && self
                .recovering_requests
                .contains(&recovery_request_key(event))
        {
            return true;
        }
        // 只白名单放行纯观察事件，避免新增事件类型悄悄绕过语义重建。
        if matches!(
            event.event_type,
            RuntimeEventType::TokenUsageRecorded
                | RuntimeEventType::ContextBuilt
                | RuntimeEventType::VerificationCompleted
                | RuntimeEventType::EvaluationCompleted
                | RuntimeEventType::PostTaskReviewed
        ) {
            let _timing = super::ui_timing::span("projection_unchanged");
            return true;
        }
        if event.event_type != RuntimeEventType::ProviderStreamed || event.turn_id.is_none() {
            return false;
        }
        let Some(record) = self.entries.last_mut().filter(|record| {
            !record.stable
                && record.task_id == event.task_id
                && record.turn_id == event.turn_id
                && record.projection.is_assistant_message()
        }) else {
            return false;
        };
        let _timing = super::ui_timing::span("projection_append");
        if let Some(delta) = provider_stream_text_delta(event)
            && let Some(body) = record.projection.item_mut().body.first_mut()
        {
            body.push_str(delta);
        }
        true
    }
}

pub(crate) fn history_event_operations(app: &TuiApp) -> Vec<EventOperationEntry> {
    let _timing = super::ui_timing::span("projection");
    if app.transcript.frame_cache_enabled
        && let Some(cache) = app.transcript.frame_operations.borrow().as_ref()
        && cache.revision == app.transcript.revision
        && cache.anchors == app.transcript.history.command_anchors
    {
        return cache.entries.clone();
    }
    let _rebuild_timing = super::ui_timing::span("projection_rebuild");
    let entries = event_operation_entries_with_boundary(
        &app.events,
        &app.transcript.history.committed_event_ids,
        &app.transcript.history.command_anchors,
        app.transcript.compact_tools || app.transcript.fullscreen,
        &app.transcript
            .expanded_operations
            .union(&app.transcript.collapsed_operations)
            .cloned()
            .chain(app.tool_detail.as_ref().map(|detail| detail.id.clone()))
            .collect(),
    );
    if app.transcript.frame_cache_enabled {
        let mut recovering_requests = HashSet::new();
        for event in &app.events {
            update_recovering_requests(&mut recovering_requests, event);
        }
        *app.transcript.frame_operations.borrow_mut() = Some(HistoryProjectionCache {
            revision: app.transcript.revision,
            event_count: app.events.len(),
            anchors: app.transcript.history.command_anchors.clone(),
            entries: entries.clone(),
            recovering_requests,
        });
    }
    entries
}

fn event_operation_entries_with_boundary(
    events: &[RuntimeEvent],
    committed: &HashSet<EventId>,
    command_anchors: &HashSet<EventId>,
    semantic_groups: bool,
    operation_boundaries: &HashSet<OperationId>,
) -> Vec<EventOperationEntry> {
    let mut typed_events = events.iter().collect::<Vec<_>>();
    typed_events.sort_by_key(|event| event.sequence_no);

    let mut items: Vec<EventOperationEntry> = Vec::new();
    let mut indexes = ProjectionIndexes::default();
    let mut process_updates = HashMap::new();
    let mut subagent_updates = HashMap::new();
    let mut turns_with_user_steps = HashSet::<TurnId>::new();
    let mut covered_user_step_tools = HashSet::<OperationId>::new();
    let mut terminal_notices = terminal::notices(&typed_events);
    for event in typed_events {
        update_recovering_requests(&mut indexes.recovering_requests, event);
        if event.event_type.is_task_terminal() {
            for record in &mut items {
                if event.task_id.is_some() && record.task_id == event.task_id
                    || event.task_id.is_none()
                        && event.turn_id.is_some()
                        && record.turn_id == event.turn_id
                {
                    record.stable = true;
                }
            }
        }
        match event.event_type {
            RuntimeEventType::RetryScheduled if event.payload.get("recovery").is_some() => {
                if event.payload["recovery"]["reset_stream"].as_bool() == Some(true)
                    && let Some(index) = event
                        .turn_id
                        .and_then(|id| indexes.streamed_assistant_items.remove(&id))
                    && let Some(record) = items.get_mut(index)
                {
                    // 已归档片段不可撤回；只标记一次，恢复期间的重复预览留在观测记录。
                    record
                        .projection
                        .item_mut()
                        .body
                        .push("[Response interrupted; retrying]".to_owned());
                    record.stable = true;
                }
            }
            RuntimeEventType::ProviderTransportFallback => {}
            RuntimeEventType::ProviderCompleted => {}
            RuntimeEventType::ProviderFailed => {}
            RuntimeEventType::TaskCreated => {
                let is_new_turn = event
                    .turn_id
                    .is_none_or(|turn_id| !indexes.visible_user_turns.contains_key(&turn_id));
                if is_new_turn && let Some(item) = user_event_transcript_item(event) {
                    if let Some(turn_id) = event.turn_id {
                        indexes.visible_user_turns.insert(turn_id, items.len());
                    }
                    items.push(EventOperationEntry::new(
                        event,
                        message_projection(item),
                        true,
                    ));
                }
            }
            RuntimeEventType::TurnQueued => {
                if let Some(turn_id) = event.turn_id
                    && !indexes.visible_user_turns.contains_key(&turn_id)
                    && let Some(item) = user_event_transcript_item(event)
                {
                    indexes.pending_user_turns.insert(turn_id, item);
                }
            }
            RuntimeEventType::TurnStarted => {
                if let Some(turn_id) = event.turn_id {
                    let pending = indexes.pending_user_turns.remove(&turn_id);
                    if !indexes.visible_user_turns.contains_key(&turn_id)
                        && let Some(item) = pending.or_else(|| user_event_transcript_item(event))
                    {
                        indexes.visible_user_turns.insert(turn_id, items.len());
                        items.push(EventOperationEntry::new(
                            event,
                            message_projection(item),
                            true,
                        ));
                    }
                }
            }
            RuntimeEventType::TurnUpdated => {
                if let Some(turn_id) = event.turn_id
                    && !indexes.visible_user_turns.contains_key(&turn_id)
                    && let Some(item) = user_event_transcript_item(event)
                {
                    indexes.pending_user_turns.insert(turn_id, item);
                }
            }
            RuntimeEventType::TurnCancelled => {
                if let Some(turn_id) = event.turn_id {
                    indexes.pending_user_turns.remove(&turn_id);
                }
            }
            RuntimeEventType::ProviderStreamed => {
                if indexes
                    .recovering_requests
                    .contains(&recovery_request_key(event))
                {
                    continue;
                }
                let Some(delta) = provider_stream_text_delta(event) else {
                    continue;
                };
                let Some(turn_id) = event.turn_id else {
                    continue;
                };
                if let Some(index) = indexes.streamed_assistant_items.get(&turn_id).copied() {
                    if let Some(record) = items.get_mut(index)
                        && let Some(body) = record.projection.item_mut().body.first_mut()
                    {
                        body.push_str(delta);
                    }
                } else {
                    let index = items.len();
                    items.push(EventOperationEntry::new(
                        event,
                        message_projection(TranscriptItem {
                            role: TranscriptRole::Assistant,
                            title: "Golutra".to_owned(),
                            body: vec![delta.to_owned()],
                        }),
                        false,
                    ));
                    indexes.streamed_assistant_items.insert(turn_id, index);
                }
            }
            RuntimeEventType::AssistantMessage => {
                // UserStep 已经按回合冻结了可见短句。整段 AssistantMessage 是模型历史，
                // 不能再覆盖或追加到用户 transcript。
                let sealed_by_user_steps = event.turn_id.is_some_and(|turn_id| {
                    turns_with_user_steps.contains(&turn_id)
                        && assistant_turn_already_split_by_tools(&items, turn_id)
                });
                if sealed_by_user_steps {
                    if let Some(index) = event
                        .turn_id
                        .and_then(|turn_id| indexes.streamed_assistant_items.remove(&turn_id))
                        && let Some(record) = items.get_mut(index)
                    {
                        record.stable = true;
                    }
                    continue;
                }
                // 只有在工具已经把同一 turn 切成多段之后，才拒绝用整段 content 覆盖。
                // 单纯的流式收口（Hello → Hello world.）仍应替换当前这一条。
                let sealed_split = event
                    .turn_id
                    .is_some_and(|turn_id| assistant_turn_already_split_by_tools(&items, turn_id));
                let streamed = event
                    .turn_id
                    .and_then(|turn_id| indexes.streamed_assistant_items.remove(&turn_id));
                if let Some(index) = streamed {
                    if let Some(record) = items.get_mut(index) {
                        if !sealed_split && let Some(item) = assistant_event_transcript_item(event)
                        {
                            record.projection = message_projection(item);
                        }
                        record.stable = true;
                    }
                } else if !sealed_split && let Some(item) = assistant_event_transcript_item(event) {
                    items.push(EventOperationEntry::new(
                        event,
                        message_projection(item),
                        true,
                    ));
                }
            }
            RuntimeEventType::ToolStarted => {
                // 工具批次开始后，同一 turn 的后续模型文本必须另起一条，
                // 否则步间短句会被拼进同一条助手消息。
                if let Some(turn_id) = event.turn_id
                    && let Some(index) = indexes.streamed_assistant_items.remove(&turn_id)
                    && let Some(record) = items.get_mut(index)
                {
                    record.stable = true;
                }
                if operation_id_from_event(event)
                    .is_some_and(|id| covered_user_step_tools.contains(&id))
                {
                    continue;
                }
                if let Some(projection) = tool_started_projection(event) {
                    let index = items.len();
                    if let Some(id) = projection.id().cloned() {
                        indexes.active_tools.insert(id, index);
                    }
                    items.push(EventOperationEntry::new(event, projection, false));
                }
            }
            RuntimeEventType::ToolProgress => {
                if let Some(id) = operation_id_from_event(event)
                    && let Some(index) = indexes.active_tools.get(&id).copied()
                    && let Some(record) = items.get_mut(index)
                {
                    update_tool_progress(&mut record.projection, event);
                }
            }
            RuntimeEventType::ProcessUpdated => {
                if let Some(process_id) = event.payload.get("process_id").and_then(Value::as_str) {
                    process_updates.insert(process_id, event);
                }
                project_process_update(&mut items, event, committed);
            }
            RuntimeEventType::SubagentUpdated => {
                if let Some(id) = event.payload.get("tool_call_id").and_then(Value::as_str) {
                    subagent_updates.insert(id, event);
                }
                project_subagent_update(&mut items, event, committed);
            }
            RuntimeEventType::ToolCompleted => {
                if let Some(id) = operation_id_from_event(event)
                    && covered_user_step_tools.contains(&id)
                {
                    indexes.active_tools.remove(&id);
                    continue;
                }
                if let Some(mut projection) = tool_operation_projection(event) {
                    if let Some(id) = projection.id().cloned()
                        && let Some(index) = indexes.active_tools.remove(&id)
                    {
                        if semantic_groups {
                            let original = items[index].projection.item(true);
                            if let Some(start) =
                                original.body.iter().position(|line| line == "Arguments")
                            {
                                match &mut projection {
                                    OperationProjection::ToolActivity { details, .. }
                                    | OperationProjection::FileChange { details, .. } => {
                                        details.push("Arguments".to_owned());
                                        details.extend(
                                            original.body[start + 1..]
                                                .iter()
                                                .take_while(|line| {
                                                    !matches!(line.as_str(), "Output" | "Facts")
                                                })
                                                .cloned(),
                                        );
                                    }
                                    _ => {}
                                }
                            }
                        }
                        items[index].projection = projection;
                        items[index].stable = !(items[index].terminal_wait
                            && items[index].projection.item(false).role
                                == TranscriptRole::Activity);
                    } else {
                        items.push(EventOperationEntry::new(event, projection, true));
                    }
                }
                if let Some(process_id) = event
                    .payload
                    .pointer("/envelope/structured_facts/process_id")
                    .and_then(Value::as_str)
                    && let Some(update) = process_updates.get(process_id)
                    && update.payload.get("terminal") == Some(&Value::Bool(true))
                {
                    project_process_update(&mut items, update, committed);
                }
                if let Some(id) = operation_id_from_event(event)
                    && let Some(update) = subagent_updates.get(id.0.as_str())
                {
                    project_subagent_update(&mut items, update, committed);
                }
            }
            RuntimeEventType::UserStep => {
                let Some(step) = user_step_from_event(event) else {
                    continue;
                };
                // 全屏用真实工具身份分组，已有完整工具事件时不让上层混合摘要覆盖执行边界。
                if semantic_groups
                    && let UserStepKind::ToolBatch { tools, .. } = &step.kind
                    && !tools.is_empty()
                    && tools.iter().all(|tool| {
                        items.iter().any(|entry| {
                            entry.projection.id()
                                == Some(&OperationId(tool.tool_call_id.to_string()))
                        })
                    })
                {
                    continue;
                }
                let turn_id = event.turn_id.unwrap_or(step.turn_id);
                turns_with_user_steps.insert(turn_id);
                // 冻结后的批次不能被晚到的展示摘要重写；原始工具终态仍逐项保留。
                if let UserStepKind::ToolBatch { tools, .. } = &step.kind
                    && items.iter().any(|entry| {
                        entry
                            .event_ids
                            .iter()
                            .any(|id| committed.contains(id) || command_anchors.contains(id))
                            && tools.iter().any(|tool| {
                                entry.projection.id()
                                    == Some(&OperationId(tool.tool_call_id.to_string()))
                            })
                    })
                {
                    continue;
                }
                apply_user_step(
                    event,
                    &step,
                    turn_id,
                    &mut items,
                    &mut indexes,
                    &mut covered_user_step_tools,
                );
            }
            _ => {
                let item = if event.task_id.is_some()
                    && (event.event_type.is_task_terminal()
                        || event.event_type == RuntimeEventType::LoopDecided)
                {
                    terminal_notices.remove(&event.id)
                } else {
                    status_event_transcript_item(event)
                };
                if let Some(item) = item {
                    items.push(EventOperationEntry::new(
                        event,
                        notice_projection(item),
                        true,
                    ));
                }
            }
        }
    }
    coalesce_completed_tool_batches(
        coalesce_terminal_waits(items, committed, command_anchors, operation_boundaries),
        committed,
        command_anchors,
        semantic_groups,
        operation_boundaries,
    )
}

fn coalesce_terminal_waits(
    items: Vec<EventOperationEntry>,
    committed: &HashSet<EventId>,
    anchors: &HashSet<EventId>,
    boundaries: &HashSet<OperationId>,
) -> Vec<EventOperationEntry> {
    let mut result: Vec<EventOperationEntry> = Vec::with_capacity(items.len());
    for mut item in items {
        if let Some(previous) = result.last_mut()
            && previous.terminal_wait
            && item.terminal_wait
            && previous.process_id.is_some()
            && previous.process_id == item.process_id
            && previous.turn_id == item.turn_id
            && !previous
                .event_ids
                .iter()
                .chain(&item.event_ids)
                .any(|id| committed.contains(id) || anchors.contains(id))
            && !previous
                .projection
                .id()
                .into_iter()
                .chain(item.projection.id())
                .any(|id| boundaries.contains(id))
            && matches!(
                previous.projection.item(false).role,
                TranscriptRole::Activity | TranscriptRole::Success
            )
            && matches!(
                item.projection.item(false).role,
                TranscriptRole::Activity | TranscriptRole::Success
            )
        {
            if let OperationProjection::ToolActivity { id, .. } = &mut item.projection
                && let Some(original_id) = previous.projection.id()
            {
                *id = original_id.clone();
            }
            previous.event_ids.extend(item.event_ids);
            previous.projection = item.projection;
            previous.stable = item.stable;
        } else {
            if let Some(previous) = result.last_mut()
                && previous.terminal_wait
            {
                previous.stable = true;
            }
            result.push(item);
        }
    }
    result
}

fn plain_projection(item: TranscriptItem) -> OperationProjection {
    match item.role {
        TranscriptRole::User | TranscriptRole::Assistant | TranscriptRole::CommandResult => {
            message_projection(item)
        }
        _ => notice_projection(item),
    }
}

pub(crate) fn message_projection(item: TranscriptItem) -> OperationProjection {
    OperationProjection::Message { item }
}

pub(crate) fn notice_projection(item: TranscriptItem) -> OperationProjection {
    OperationProjection::Notice { item }
}

fn user_step_from_event(event: &RuntimeEvent) -> Option<UserStep> {
    event
        .payload
        .get("step")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
}

fn assistant_turn_already_split_by_tools(items: &[EventOperationEntry], turn_id: TurnId) -> bool {
    let mut saw_assistant = false;
    let mut saw_tool_after_assistant = false;
    for record in items {
        if record.turn_id != Some(turn_id) {
            continue;
        }
        if record.projection.is_assistant_message() {
            saw_assistant = true;
        } else if saw_assistant
            && matches!(
                record.projection,
                OperationProjection::ToolActivity { .. } | OperationProjection::FileChange { .. }
            )
        {
            saw_tool_after_assistant = true;
        }
    }
    saw_assistant && saw_tool_after_assistant
}

fn remove_projected_item(
    index: usize,
    items: &mut Vec<EventOperationEntry>,
    indexes: &mut ProjectionIndexes,
) {
    items.remove(index);
    for position in indexes.visible_user_turns.values_mut() {
        if *position > index {
            *position = position.saturating_sub(1);
        }
    }
    for position in indexes.streamed_assistant_items.values_mut() {
        if *position > index {
            *position = position.saturating_sub(1);
        }
    }
    for position in indexes.active_tools.values_mut() {
        if *position > index {
            *position = position.saturating_sub(1);
        }
    }
}

fn apply_user_step(
    event: &RuntimeEvent,
    step: &UserStep,
    turn_id: TurnId,
    items: &mut Vec<EventOperationEntry>,
    indexes: &mut ProjectionIndexes,
    covered_user_step_tools: &mut HashSet<OperationId>,
) {
    match &step.kind {
        UserStepKind::AssistantText { .. } => {
            let Some(projection) = user_step_projection(step) else {
                return;
            };
            if let Some(index) = indexes.streamed_assistant_items.remove(&turn_id) {
                if let Some(record) = items.get_mut(index) {
                    record.projection = projection;
                    record.stable = true;
                }
                return;
            }
            // AssistantMessage / 流式文本可能先于 UserStep 到达；收口同一回合尚未被工具切开的助手行。
            if let Some(index) = trailing_assistant_index(items, turn_id)
                && let Some(record) = items.get_mut(index)
            {
                record.projection = projection;
                record.stable = true;
                return;
            }
            if assistant_text_already_visible(items, turn_id, &projection) {
                return;
            }
            items.push(EventOperationEntry::new(event, projection, true));
        }
        UserStepKind::ToolBatch { tools, .. } => {
            let Some(mut projection) = user_step_projection(step) else {
                return;
            };
            let mut replace_at = None;
            let mut retained_details = Vec::new();
            let mut retained_errors = Vec::new();
            for tool in tools {
                let id = OperationId(tool.tool_call_id.to_string());
                covered_user_step_tools.insert(id.clone());
                let index = indexes.active_tools.remove(&id).or_else(|| {
                    items
                        .iter()
                        .position(|record| record.projection.id() == Some(&id))
                });
                if let Some(index) = index {
                    let original = &items[index].projection;
                    let expanded = original.item(true);
                    retained_details.push(expanded.title);
                    retained_details.extend(expanded.body);
                    if !original.is_successful_completed_tool() {
                        retained_errors.extend(original.item(false).body);
                    }
                    replace_at =
                        Some(replace_at.map_or(index, |current: usize| current.min(index)));
                }
            }
            // UserStep 只替换摘要，不能销毁真实工具结果；默认保留错误，Ctrl+O 可展开原始证据。
            if let OperationProjection::ToolActivity { item, details, .. } = &mut projection {
                item.body.extend(retained_errors);
                if !retained_details.is_empty() {
                    details.push("Output".to_owned());
                    details.extend(retained_details);
                }
            }
            let Some(first_index) = replace_at else {
                items.push(EventOperationEntry::new(event, projection, true));
                return;
            };
            let mut merged_ids = vec![event.id];
            if let Some(record) = items.get_mut(first_index) {
                merged_ids.extend(record.event_ids.iter().copied());
                record.projection = projection;
                record.stable = true;
            }
            let mut remove = tools
                .iter()
                .filter_map(|tool| {
                    let id = OperationId(tool.tool_call_id.to_string());
                    items
                        .iter()
                        .position(|record| record.projection.id() == Some(&id))
                })
                .filter(|index| *index != first_index)
                .collect::<Vec<_>>();
            remove.sort_unstable();
            remove.dedup();
            for index in remove.into_iter().rev() {
                if let Some(removed) = items.get(index) {
                    merged_ids.extend(removed.event_ids.iter().copied());
                }
                remove_projected_item(index, items, indexes);
            }
            if let Some(record) = items.get_mut(first_index) {
                merged_ids.sort();
                merged_ids.dedup();
                record.event_ids = merged_ids;
            }
        }
    }
}

fn assistant_text_already_visible(
    items: &[EventOperationEntry],
    turn_id: TurnId,
    projection: &OperationProjection,
) -> bool {
    let OperationProjection::Message { item } = projection else {
        return false;
    };
    items.iter().any(|record| {
        record.turn_id == Some(turn_id)
            && record.projection.is_assistant_message()
            && record.projection.item(false).body == item.body
    })
}

fn trailing_assistant_index(items: &[EventOperationEntry], turn_id: TurnId) -> Option<usize> {
    let index = items.iter().rposition(|record| {
        record.turn_id == Some(turn_id) && record.projection.is_assistant_message()
    })?;
    let has_tool_after = items[index + 1..].iter().any(|record| {
        record.turn_id == Some(turn_id)
            && matches!(
                record.projection,
                OperationProjection::ToolActivity { .. } | OperationProjection::FileChange { .. }
            )
    });
    (!has_tool_after).then_some(index)
}

fn operation_id_from_event(event: &RuntimeEvent) -> Option<OperationId> {
    event
        .payload
        .get("tool_call_id")
        .or_else(|| event.payload.pointer("/envelope/tool_call_id"))
        .and_then(Value::as_str)
        .map(|value| OperationId(value.to_owned()))
}

fn tool_started_projection(event: &RuntimeEvent) -> Option<OperationProjection> {
    let tool_name = event.payload.get("tool_name")?.as_str()?;
    let id = operation_id_from_event(event)?;
    let arguments = event.payload.get("arguments");
    let invocation = tool_invocation(tool_name, arguments);
    let mut details = Vec::new();
    if !invocation.is_empty() {
        details.push(invocation.clone());
    }
    if let Some(arguments) = arguments {
        details.push("Arguments".to_owned());
        details.extend(pretty_json_lines(arguments, 20));
    }
    Some(OperationProjection::ToolActivity {
        id,
        item: TranscriptItem {
            role: TranscriptRole::Activity,
            title: if tool_name == "shell_session"
                && arguments
                    .and_then(|args| args.get("action"))
                    .and_then(Value::as_str)
                    == Some("read")
            {
                "Reading terminal output".to_owned()
            } else if invocation.is_empty()
                || matches!(
                    tool_name,
                    "shell" | "shell_session" | "subagent" | "delegate_task"
                )
            {
                running_tool_title(tool_name)
            } else {
                format!(
                    "{} {}",
                    running_tool_title(tool_name),
                    if matches!(tool_name, "read_file" | "write_file" | "edit_file") {
                        display_file_name(&invocation)
                    } else {
                        invocation.clone()
                    }
                )
            },
            body: Vec::new(),
        },
        details,
    })
}

fn update_tool_progress(projection: &mut OperationProjection, event: &RuntimeEvent) {
    let Some(progress) = event.payload.get("progress") else {
        return;
    };
    if progress.get("phase").and_then(Value::as_str) != Some("output") {
        return;
    }
    let elapsed_ms = progress
        .get("elapsed_ms")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let output_bytes = progress
        .get("output_bytes")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let output_lines = progress
        .get("output_lines")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let stream = progress
        .get("detail")
        .and_then(Value::as_str)
        .unwrap_or("output");
    let progress_line = format!(
        "{stream} · {} · {} · {}",
        plural_count(output_lines, "line", "lines"),
        format_bytes(output_bytes),
        format_millis(elapsed_ms)
    );
    match projection {
        OperationProjection::ToolActivity { details, .. }
        | OperationProjection::FileChange { details, .. } => {
            details.retain(|line| !line.starts_with("stdout ·") && !line.starts_with("│ "));
            details.push(progress_line);
            if let Some(excerpt) = progress.get("output_excerpt").and_then(Value::as_str) {
                let lines = excerpt.lines().filter(|line| !line.trim().is_empty());
                let mut lines = lines.rev().take(6).collect::<Vec<_>>();
                lines.reverse();
                details.extend(
                    lines
                        .into_iter()
                        .map(|line| format!("│ {}", bounded_text(line, 320))),
                );
            }
        }
        OperationProjection::Message { .. } | OperationProjection::Notice { .. } => {}
    }
}

fn tool_operation_projection(event: &RuntimeEvent) -> Option<OperationProjection> {
    let id = operation_id_from_event(event)
        .unwrap_or_else(|| OperationId(format!("event:{}", event.sequence_no)));
    let Some(envelope) = event.payload.get("envelope") else {
        return tool_event_transcript_item(event).map(notice_projection);
    };
    let tool_name = envelope
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("tool");
    let status = tool_result_status(event);
    let facts = envelope.get("structured_facts");
    let invocation = tool_invocation(tool_name, facts);
    let summary = envelope
        .get("summary")
        .and_then(Value::as_str)
        .or_else(|| event.payload.get("summary").and_then(Value::as_str))
        .unwrap_or("tool completed");
    let metrics = event.payload.get("metrics");
    let mut title = completed_tool_title_with_object(
        tool_name,
        status,
        (!invocation.is_empty()).then_some(invocation.as_str()),
    );
    let process_state = facts
        .and_then(|facts| facts.get("process_state"))
        .and_then(Value::as_str);
    let reading_output = tool_name == "shell_session"
        && facts
            .and_then(|facts| facts.get("action"))
            .and_then(Value::as_str)
            == Some("read");
    let running_process = !reading_output
        && matches!(tool_name, "shell" | "shell_session")
        && process_state == Some("running");
    if reading_output && status == ToolResultStatus::Ok {
        title = "Read terminal output".to_owned();
    } else if running_process {
        title = if tool_name == "shell" {
            "Background terminal running"
        } else {
            "Waiting for background terminal"
        }
        .to_owned();
    } else if tool_name == "shell" && process_state == Some("exited") {
        title = "ran".to_owned();
    }
    let delegated = matches!(tool_name, "subagent" | "delegate_task");
    let child_status = facts
        .and_then(|facts| facts.get("child_status"))
        .and_then(Value::as_str);
    let partial_child = delegated
        && child_status == Some("partial")
        && status == ToolResultStatus::Error
        && facts.and_then(|facts| facts.get("error")).is_none();
    let running_child = delegated
        && status == ToolResultStatus::Ok
        && matches!(child_status, Some("running" | "aborting"));
    if partial_child {
        title = "subagent · verification incomplete".to_owned();
    } else if running_child {
        title = if child_status == Some("aborting") {
            "Stopping subagent"
        } else {
            "Subagent running"
        }
        .to_owned();
    }
    let mut body = Vec::new();
    let mut details = Vec::new();
    if delegated
        && let Some(results) = facts
            .and_then(|facts| facts.get("child_results"))
            .and_then(Value::as_array)
    {
        let pending = facts
            .and_then(|facts| facts.get("child_pending_ids"))
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        title = format!(
            "Subagents · {} finished · {pending} running",
            results.len().saturating_sub(pending)
        );
    }
    if tool_name == "shell_session" && facts.and_then(|facts| facts.get("total_count")).is_some() {
        title = "Background terminals".to_owned();
        body.push(format!(
            "{} running · {} total",
            facts.unwrap()["running_count"],
            facts.unwrap()["total_count"]
        ));
    }
    if !invocation.is_empty() {
        details.push(invocation);
    }
    if let Some(line) = tool_metrics_line(metrics, facts) {
        details.push(line);
    }
    if !delegated
        && !matches!(tool_name, "shell" | "shell_session")
        && facts
            .and_then(|value| value.get("workspace_changes_known"))
            .and_then(Value::as_bool)
            == Some(false)
    {
        details.push("workspace changes unknown".to_owned());
    }
    if status != ToolResultStatus::Ok && !partial_child && !summary.trim().is_empty() {
        if let Some(diagnostic) = invalid_tool_request_diagnostic(event) {
            title = format!("Failed · {tool_name}");
            body.extend(bounded_output_lines(&diagnostic, 3));
        } else {
            body.push(summary.to_owned());
        }
    }
    if delegated {
        if let Some(issues) = facts
            .and_then(|facts| facts.get("child_verification_issues"))
            .and_then(Value::as_array)
        {
            for issue in issues.iter().take(2) {
                if let Some(reason) = issue.get("reason").and_then(Value::as_str) {
                    body.push(bounded_text(reason, 240));
                }
            }
        }
        details.push("Child details".to_owned());
        if facts
            .and_then(|facts| facts.get("workspace_changes_known"))
            .and_then(Value::as_bool)
            == Some(false)
        {
            details.push("workspace changes unknown".to_owned());
        }
        for key in [
            "child_session_id",
            "child_results",
            "child_pending_ids",
            "child_workspace_path",
            "child_isolation",
            "child_execution_status",
            "child_verification_status",
            "child_diagnostic",
            "child_verification_issues",
            "child_result_has_more",
            "child_result_next_offset",
        ] {
            if let Some(value) = facts
                .and_then(|facts| facts.get(key))
                .filter(|value| !value.is_null())
            {
                details.push(format!(
                    "{key}: {}",
                    value
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| value.to_string())
                ));
            }
        }
    }

    let excerpt = envelope
        .get("model_visible_excerpt")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let output_lines = if matches!(tool_name, "shell" | "shell_session") {
        excerpt
            .lines()
            .take(40)
            .map(|line| bounded_text(line, 500))
            .collect()
    } else {
        bounded_output_lines(excerpt, 40)
    };
    let changes = operation_file_changes(event);
    if !changes.is_empty() {
        let mut item = file_change_item(
            &changes,
            if partial_child {
                ToolResultStatus::Ok
            } else {
                status
            },
        );
        if partial_child {
            item.title = format!("{title} · {}", item.title);
            item.role = TranscriptRole::Warning;
        }
        body.append(&mut item.body);
        item.body = body;
        if !output_lines.is_empty() {
            details.push("Output".to_owned());
            details.extend(output_lines);
        }
        details.extend(file_change_details(event, &changes));
        return Some(OperationProjection::FileChange { id, item, details });
    }

    if !output_lines.is_empty() {
        details.push("Output".to_owned());
        details.extend(output_lines);
    }
    Some(OperationProjection::ToolActivity {
        id,
        item: TranscriptItem {
            role: if partial_child {
                TranscriptRole::Warning
            } else if running_child || running_process {
                TranscriptRole::Activity
            } else {
                tool_status_role(status)
            },
            title,
            body,
        },
        details,
    })
}

fn project_process_update(
    items: &mut Vec<EventOperationEntry>,
    event: &RuntimeEvent,
    committed: &HashSet<EventId>,
) {
    let Some(process_id) = event.payload.get("process_id").and_then(Value::as_str) else {
        return;
    };
    let call_id = process_id.strip_prefix("proc-").unwrap_or(process_id);
    let terminal_id = format!("terminal:{process_id}");
    let original = items
        .iter()
        .rposition(|record| record.projection.id().is_some_and(|id| id.0 == call_id));
    let terminal = event.payload.get("terminal").and_then(Value::as_bool) == Some(true);
    let existing_notice = items
        .iter()
        .rposition(|record| record.projection.id().is_some_and(|id| id.0 == terminal_id));
    let target = existing_notice.or(original);
    let Some(target) = target else {
        return;
    };
    let archived = items[target]
        .event_ids
        .iter()
        .any(|id| committed.contains(id));
    let append_notice = archived
        && terminal
        && existing_notice.is_none()
        && items[target].projection.item(false).role == TranscriptRole::Activity;
    // A late process event must not relabel a completed foreground command as
    // a background job. Still refresh its process details below.
    let completed_foreground = items[target].projection.item(false).title == "ran";
    let state = event
        .payload
        .get("process_state")
        .and_then(Value::as_str)
        .unwrap_or("running");
    let role = match state {
        "running" => TranscriptRole::Activity,
        "exited" => TranscriptRole::Success,
        "cancelled" | "terminated" => TranscriptRole::System,
        _ => TranscriptRole::Error,
    };
    let title = match state {
        "running" => "Background terminal running",
        "exited" if completed_foreground => "ran",
        "exited" => "Background terminal completed",
        "cancelled" => "Background terminal cancelled",
        "terminated" => "Background terminal stopped",
        "timed_out" => "Background terminal timed out",
        _ => "Background terminal failed",
    };
    let command = event
        .payload
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or(process_id);
    let mut details = vec![command.to_owned()];
    if let Some(metrics) = tool_metrics_line(None, Some(&event.payload)) {
        details.push(metrics);
    }
    details.push("Output".to_owned());
    if let Some(output) = event.payload.get("output_excerpt").and_then(Value::as_str) {
        details.extend(output.lines().map(str::to_owned));
    }
    details.push(format!("Process: {process_id}"));
    details.push(format!("State: {state}"));
    if event.payload.get("workspace_scan_pending") == Some(&Value::Bool(true)) && terminal {
        details.push("Workspace change inspection is pending".to_owned());
    }
    let projection = OperationProjection::ToolActivity {
        id: OperationId(if append_notice {
            terminal_id
        } else {
            items[target]
                .projection
                .id()
                .map(|id| id.0.clone())
                .unwrap_or(terminal_id)
        }),
        item: TranscriptItem {
            role,
            title: title.to_owned(),
            body: Vec::new(),
        },
        details,
    };
    if append_notice {
        if let Some(original) = original
            && let OperationProjection::ToolActivity {
                details: original_details,
                ..
            } = &mut items[original].projection
            && let OperationProjection::ToolActivity { details, .. } = &projection
        {
            original_details.clone_from(details);
        }
        items.push(EventOperationEntry::new(event, projection, true));
    } else {
        items[target].projection = projection;
        if !archived && !items[target].event_ids.contains(&event.id) {
            items[target].event_ids.push(event.id);
        }
    }
}

fn project_subagent_update(
    items: &mut Vec<EventOperationEntry>,
    event: &RuntimeEvent,
    committed: &HashSet<EventId>,
) {
    let Some(call_id) = event.payload.get("tool_call_id").and_then(Value::as_str) else {
        return;
    };
    let notice_id = format!("subagent:{call_id}");
    let original = items
        .iter()
        .rposition(|entry| entry.projection.id().is_some_and(|id| id.0 == call_id));
    let existing = items
        .iter()
        .rposition(|entry| entry.projection.id().is_some_and(|id| id.0 == notice_id));
    let target = existing.or(original);
    if target.is_none() {
        return;
    }
    let archived = target.is_some_and(|index| {
        items[index]
            .event_ids
            .iter()
            .any(|id| committed.contains(id))
    });
    let append = target.is_none()
        || (archived
            && existing.is_none()
            && target.is_some_and(|index| {
                items[index].projection.item(false).role == TranscriptRole::Activity
            }));
    let facts = &event.payload["facts"];
    let status = facts
        .get("child_status")
        .and_then(Value::as_str)
        .unwrap_or("completed");
    let role = match status {
        "completed" => TranscriptRole::Success,
        "failed" => TranscriptRole::Error,
        "cancelled" | "interrupted" => TranscriptRole::System,
        _ => TranscriptRole::Warning,
    };
    let mut details = vec![
        event
            .payload
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    ];
    if let Some(child) = facts.get("child_session_id").and_then(Value::as_str) {
        details.push(format!("Child: {child}"));
    }
    if let Some(content) = event.payload.get("content").and_then(Value::as_str) {
        details.extend(content.lines().map(str::to_owned));
    }
    let projection = OperationProjection::ToolActivity {
        id: OperationId(if append || existing.is_some() {
            notice_id
        } else {
            call_id.to_owned()
        }),
        item: TranscriptItem {
            role,
            title: format!("Subagent {status}"),
            body: Vec::new(),
        },
        details,
    };
    if append {
        if let Some(index) = original
            && let OperationProjection::ToolActivity {
                details: original_details,
                ..
            } = &mut items[index].projection
            && let OperationProjection::ToolActivity { details, .. } = &projection
        {
            original_details.clone_from(details);
        }
        items.push(EventOperationEntry::new(event, projection, true));
    } else if let Some(index) = target {
        items[index].projection = projection;
        items[index].stable = true;
        if !archived && !items[index].event_ids.contains(&event.id) {
            items[index].event_ids.push(event.id);
        }
    }
}

fn running_tool_title(tool_name: &str) -> String {
    match tool_name {
        "shell" => "Running".to_owned(),
        "shell_session" => "Waited for background terminal".to_owned(),
        "subagent" | "delegate_task" => "Running subagent".to_owned(),
        "read_file" => "Reading".to_owned(),
        "list_dir" => "Listing".to_owned(),
        "rg_search" | "symbol_search" | "find_references" => "Searching".to_owned(),
        "write_file" | "edit_file" => "Editing".to_owned(),
        other => format!("Calling {other}"),
    }
}

const DEFAULT_TOOL_PREVIEW_LINES: usize = 5;

fn default_tool_preview(details: &[String], show_output: bool, running: bool) -> Vec<String> {
    let mut preview = details
        .iter()
        // 参数仍在详情中；默认保留命令与执行计数，并在下面附上少量实际输出。
        .take_while(|line| {
            !matches!(
                line.as_str(),
                "Arguments" | "Output" | "Facts" | "Child details"
            )
        })
        .filter(|line| {
            let line = line.as_str();
            !line.is_empty() && !line.starts_with("│ ")
        })
        .take(DEFAULT_TOOL_PREVIEW_LINES)
        .cloned()
        .collect::<Vec<_>>();
    if show_output && let Some(start) = details.iter().position(|line| line == "Output") {
        let output = details[start + 1..]
            .iter()
            .take_while(|line| {
                !line.starts_with("Process: ")
                    && !line.starts_with("State: ")
                    && line.as_str() != "Facts"
            })
            .collect::<Vec<_>>();
        let skip = if running {
            output.len().saturating_sub(5)
        } else {
            0
        };
        preview.extend(
            output
                .iter()
                .skip(skip)
                .take(5)
                .map(|line| format!("│ {line}")),
        );
        if output.len() > 5 {
            let tail = if running {
                0
            } else {
                output.len().saturating_sub(5).min(5)
            };
            let omitted = output.len().saturating_sub(5 + tail);
            if omitted > 0 {
                preview.push(format!("… {omitted} more lines · Ctrl+O to view"));
            }
            preview.extend(
                output[output.len() - tail..]
                    .iter()
                    .map(|line| format!("│ {line}")),
            );
        }
    }
    preview
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            if line.starts_with("  └ ") || line.starts_with("    ") || line.starts_with("│ ") {
                line
            } else if index == 0 {
                format!("  └ {line}")
            } else {
                format!("    {line}")
            }
        })
        .collect()
}

fn completed_tool_title(tool_name: &str, status: ToolResultStatus) -> String {
    completed_tool_title_with_object(tool_name, status, None)
}

fn completed_tool_title_with_object(
    tool_name: &str,
    status: ToolResultStatus,
    object: Option<&str>,
) -> String {
    match status {
        ToolResultStatus::Blocked => return "Blocked".to_owned(),
        ToolResultStatus::Cancelled => return "Cancelled".to_owned(),
        ToolResultStatus::Timeout => return "Timed out".to_owned(),
        ToolResultStatus::Error => return "Failed".to_owned(),
        ToolResultStatus::Ok => {}
    }
    if matches!(tool_name, "shell_session") {
        return if object.is_some_and(|value| value.contains('\n')) {
            "Interacted with background terminal".to_owned()
        } else {
            "Waited for background terminal".to_owned()
        };
    }
    if matches!(tool_name, "subagent" | "delegate_task") {
        return "subagent".to_owned();
    }
    let kind = ToolSummaryKind::from_tool_name(tool_name);
    match object.map(str::trim).filter(|value| !value.is_empty()) {
        Some(object) => {
            let object = match kind {
                ToolSummaryKind::Read | ToolSummaryKind::Edited => display_file_name(object),
                ToolSummaryKind::Ran => String::new(),
                _ => object.to_owned(),
            };
            if object.is_empty() {
                kind.label().to_owned()
            } else {
                format!("{} {object}", kind.label())
            }
        }
        None if kind == ToolSummaryKind::Other => "Tool Completed".to_owned(),
        None => kind.label().to_owned(),
    }
}

fn display_file_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
        .to_owned()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolSummaryKind {
    Read,
    Listed,
    Searched,
    Edited,
    Ran,
    Other,
}

impl ToolSummaryKind {
    fn is_exploration(self) -> bool {
        matches!(self, Self::Read | Self::Listed | Self::Searched)
    }
    fn from_tool_name(tool_name: &str) -> Self {
        match tool_name {
            "read_file" => Self::Read,
            "list_dir" => Self::Listed,
            "rg_search" | "symbol_search" | "find_references" => Self::Searched,
            "write_file" | "edit_file" | "apply_patch" => Self::Edited,
            "shell" | "shell_session" => Self::Ran,
            _ => Self::Other,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Listed => "listed",
            Self::Searched => "searched",
            Self::Edited => "edited",
            Self::Ran => "ran",
            Self::Other => "used",
        }
    }

    fn noun(self, count: usize) -> &'static str {
        match (self, count) {
            (Self::Read, 1) => "file",
            (Self::Read, _) => "files",
            (Self::Listed, 1) => "directory",
            (Self::Listed, _) => "directories",
            (Self::Searched, 1) => "pattern",
            (Self::Searched, _) => "patterns",
            (Self::Edited, 1) => "file",
            (Self::Edited, _) => "files",
            (Self::Ran, 1) => "shell command",
            (Self::Ran, _) => "shell commands",
            (Self::Other, 1) => "other tool",
            (Self::Other, _) => "other tools",
        }
    }
}

fn tool_activity_object(projection: &OperationProjection) -> Option<String> {
    let (item, details) = match projection {
        OperationProjection::ToolActivity { item, details, .. }
        | OperationProjection::FileChange { item, details, .. } => (item, details.as_slice()),
        OperationProjection::Message { .. } | OperationProjection::Notice { .. } => return None,
    };
    if let Some(object) = item.title.split_once(' ').map(|(_, rest)| rest.trim())
        && !object.is_empty()
        && !object.chars().next().is_some_and(|ch| ch.is_ascii_digit())
    {
        return Some(object.to_owned());
    }
    details
        .iter()
        .find(|line| {
            !line.is_empty()
                && *line != "Arguments"
                && *line != "Output"
                && !line.starts_with('{')
                && !line.starts_with("exit ")
                && !line.contains(" · ")
        })
        .cloned()
}

fn summarize_successful_tool_batch(entries: &[EventOperationEntry]) -> Option<EventOperationEntry> {
    if entries.len() < 2
        || !entries
            .iter()
            .all(|entry| entry.stable && entry.projection.is_successful_completed_tool())
    {
        return None;
    }

    let mut counts = [
        (ToolSummaryKind::Read, Vec::new()),
        (ToolSummaryKind::Listed, Vec::new()),
        (ToolSummaryKind::Searched, Vec::new()),
        (ToolSummaryKind::Edited, Vec::new()),
        (ToolSummaryKind::Ran, Vec::new()),
        (ToolSummaryKind::Other, Vec::new()),
    ];
    let mut details = Vec::new();
    let mut expanded_details = Vec::new();
    for entry in entries {
        let item = entry.projection.item(false);
        let object = tool_activity_object(&entry.projection).unwrap_or_default();
        let kind = match item.title.as_str() {
            title if title == "read" || title.starts_with("read ") => ToolSummaryKind::Read,
            title if title == "listed" || title.starts_with("listed ") => ToolSummaryKind::Listed,
            title if title == "searched" || title.starts_with("searched ") => {
                ToolSummaryKind::Searched
            }
            title if title == "edited" || title.starts_with("edited ") => ToolSummaryKind::Edited,
            title if title == "ran" || title.starts_with("ran ") => ToolSummaryKind::Ran,
            _ => ToolSummaryKind::Other,
        };
        if let Some((_, objects)) = counts.iter_mut().find(|(candidate, _)| *candidate == kind) {
            if !object.is_empty() {
                objects.push(object.to_owned());
            } else {
                objects.push(String::new());
            }
        }
        if !object.is_empty() {
            details.push(object.to_owned());
        }
        if let OperationProjection::ToolActivity { details: extra, .. }
        | OperationProjection::FileChange { details: extra, .. } = &entry.projection
        {
            expanded_details.push(item.title);
            expanded_details.extend(extra.iter().cloned());
        }
    }
    if !expanded_details.is_empty() {
        details.push("Output".to_owned());
        details.extend(expanded_details);
    }

    let parts = counts
        .iter()
        .filter(|(_, objects)| !objects.is_empty())
        .map(|(kind, objects)| {
            if objects.len() == 1 && !objects[0].is_empty() {
                let object = match kind {
                    ToolSummaryKind::Read | ToolSummaryKind::Edited => {
                        display_file_name(&objects[0])
                    }
                    ToolSummaryKind::Ran => String::new(),
                    _ => objects[0].clone(),
                };
                if object.is_empty() {
                    format!("{} {}", kind.label(), kind.noun(objects.len()))
                } else {
                    format!("{} {object}", kind.label())
                }
            } else {
                format!(
                    "{} {} {}",
                    kind.label(),
                    objects.len(),
                    kind.noun(objects.len())
                )
            }
        })
        .collect::<Vec<_>>();
    if parts.is_empty() {
        return None;
    }

    let mut first = entries[0].clone();
    first.event_ids = entries
        .iter()
        .flat_map(|entry| entry.event_ids.iter().copied())
        .collect();
    first.projection = OperationProjection::ToolActivity {
        id: first
            .projection
            .id()
            .cloned()
            .unwrap_or_else(|| OperationId(format!("batch:{}", first.id))),
        item: TranscriptItem {
            role: TranscriptRole::Success,
            title: parts.join(", "),
            body: Vec::new(),
        },
        details,
    };
    first.stable = true;
    Some(first)
}

fn coalesce_completed_tool_batches(
    items: Vec<EventOperationEntry>,
    committed: &HashSet<EventId>,
    command_anchors: &HashSet<EventId>,
    semantic_groups: bool,
    operation_boundaries: &HashSet<OperationId>,
) -> Vec<EventOperationEntry> {
    let mut coalesced = Vec::with_capacity(items.len());
    let mut index = 0;
    while index < items.len() {
        if items[index].stable && items[index].projection.is_successful_completed_tool() {
            let mut end = index + 1;
            while end < items.len()
                && items[end].stable
                && items[end].projection.is_successful_completed_tool()
                && items[end].turn_id.is_some()
                && items[end].turn_id == items[index].turn_id
                // 用户正在查看的单元不能在完成或补页时被前一个分组吞掉。
                && !items[end].projection.id().is_some_and(|id| operation_boundaries.contains(id))
                && (!semantic_groups || (items[index].tool_kind.is_exploration() && items[end].tool_kind.is_exploration()))
                // /status 等本地记录插在调用之间时，重排也不能合并越过它。
                && !items[end - 1].event_ids.iter().any(|id| command_anchors.contains(id))
                && items[end].event_ids.iter().all(|id| committed.contains(id))
                    == items[index]
                        .event_ids
                        .iter()
                        .all(|id| committed.contains(id))
            {
                end += 1;
            }
            if let Some(summary) = summarize_successful_tool_batch(&items[index..end]) {
                coalesced.push(summary);
                index = end;
                continue;
            }
        }
        coalesced.push(items[index].clone());
        index += 1;
    }
    coalesced
}

fn tool_status_role(status: ToolResultStatus) -> TranscriptRole {
    match status {
        ToolResultStatus::Ok => TranscriptRole::Success,
        ToolResultStatus::Blocked | ToolResultStatus::Timeout => TranscriptRole::Warning,
        ToolResultStatus::Cancelled => TranscriptRole::System,
        ToolResultStatus::Error => TranscriptRole::Error,
    }
}

fn tool_invocation(tool_name: &str, values: Option<&Value>) -> String {
    let Some(values) = values else {
        return String::new();
    };
    let string = |key: &str| values.get(key).and_then(Value::as_str);
    match tool_name {
        "shell" => string("command").unwrap_or_default().to_owned(),
        "shell_session" => {
            let command = string("command").unwrap_or_default();
            let stdin = string("stdin")
                .or_else(|| string("input"))
                .unwrap_or_default();
            if stdin.trim().is_empty() {
                command.to_owned()
            } else if command.trim().is_empty() {
                stdin.to_owned()
            } else {
                format!("{command}\n{stdin}")
            }
        }
        "subagent" | "delegate_task" => string("task")
            .or_else(|| string("child_task"))
            .or_else(|| string("child_status"))
            .unwrap_or_default()
            .to_owned(),
        "read_file" | "write_file" | "edit_file" | "list_dir" => {
            string("path").unwrap_or_default().to_owned()
        }
        "rg_search" => match (string("pattern"), string("path")) {
            (Some(pattern), Some(path)) => format!("{pattern} in {path}"),
            (Some(pattern), None) => pattern.to_owned(),
            _ => String::new(),
        },
        "symbol_search" => string("query").unwrap_or_default().to_owned(),
        "find_references" => string("symbol").unwrap_or_default().to_owned(),
        _ => bounded_text(&values.to_string(), 240),
    }
}

fn tool_metrics_line(metrics: Option<&Value>, facts: Option<&Value>) -> Option<String> {
    let mut parts = Vec::new();
    let exit_code = metrics
        .and_then(|value| value.get("exit_code"))
        .and_then(Value::as_i64)
        .or_else(|| {
            facts
                .and_then(|value| value.get("exit_code"))
                .and_then(Value::as_i64)
        });
    if let Some(exit_code) = exit_code {
        parts.push(format!("exit {exit_code}"));
    }
    let match_count = metrics
        .and_then(|value| value.get("match_count"))
        .and_then(Value::as_u64);
    let item_count = metrics
        .and_then(|value| value.get("item_count"))
        .and_then(Value::as_u64);
    let output_lines = metrics
        .and_then(|value| value.get("output_lines"))
        .and_then(Value::as_u64)
        .unwrap_or_default();
    if let Some(matches) = match_count {
        parts.push(plural_count(matches, "match", "matches"));
    } else if let Some(items) = item_count {
        parts.push(plural_count(items, "item", "items"));
    } else if output_lines > 0 {
        parts.push(plural_count(output_lines, "line", "lines"));
    }
    if let Some(bytes) = metrics
        .and_then(|value| value.get("output_bytes"))
        .and_then(Value::as_u64)
        .filter(|bytes| *bytes > 0)
    {
        parts.push(format_bytes(bytes));
    }
    if let Some(duration) = facts
        .and_then(|value| value.get("elapsed_ms"))
        .or_else(|| metrics.and_then(|value| value.get("duration_ms")))
        .and_then(Value::as_u64)
    {
        parts.push(format_millis(duration));
    }
    if metrics
        .and_then(|value| value.get("output_truncated"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        parts.push("truncated".to_owned());
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn file_change_details(event: &RuntimeEvent, changes: &[FileChangeSummary]) -> Vec<String> {
    let mut details = changes
        .iter()
        .map(|change| {
            let kind = match change.kind {
                FileChangeKind::Added => "added",
                FileChangeKind::Modified => "modified",
                FileChangeKind::Deleted => "deleted",
            };
            match (change.added_lines, change.removed_lines) {
                (Some(added), Some(removed)) => {
                    format!("{kind} {}  +{added} -{removed}", change.path)
                }
                _ => format!("{kind} {}", change.path),
            }
        })
        .collect::<Vec<_>>();
    if let Some(hunks) = event.payload.get("diff_hunks").and_then(Value::as_array) {
        details.push("Diff".to_owned());
        details.extend(crate::tool_preview::numbered_diff(
            hunks
                .iter()
                .filter_map(Value::as_str)
                .take(80)
                .map(ToOwned::to_owned),
        ));
    }
    if let Some(previews) = event.payload.get("diff_previews").and_then(Value::as_array) {
        details.push("Diff".to_owned());
        for preview in previews.iter().take(12) {
            let path = preview
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("file");
            if changes.len() > 1 {
                details.push(path.to_owned());
            }
            details.extend(crate::tool_preview::numbered_diff(
                preview
                    .get("lines")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .take(80)
                    .map(ToOwned::to_owned),
            ));
            if preview
                .get("truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                details.push("… diff preview truncated".to_owned());
            }
        }
    }
    details
}

fn pretty_json_lines(value: &Value, limit: usize) -> Vec<String> {
    serde_json::to_string_pretty(value)
        .unwrap_or_else(|_| value.to_string())
        .lines()
        .take(limit)
        .map(|line| bounded_text(line, 320))
        .collect()
}

fn bounded_output_lines(value: &str, limit: usize) -> Vec<String> {
    value
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(limit)
        .map(|line| bounded_text(line, 500))
        .collect()
}

fn plural_count(value: u64, singular: &str, plural: &str) -> String {
    format!("{value} {}", if value == 1 { singular } else { plural })
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1_024 {
        return format!("{bytes} B");
    }
    if bytes < 1_024 * 1_024 {
        return format!("{:.1} KiB", bytes as f64 / 1_024.0);
    }
    format!("{:.1} MiB", bytes as f64 / (1_024.0 * 1_024.0))
}

fn format_millis(milliseconds: u64) -> String {
    if milliseconds < 1_000 {
        format!("{milliseconds} ms")
    } else {
        format!("{:.1} s", milliseconds as f64 / 1_000.0)
    }
}

fn bounded_text(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let prefix = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn provider_stream_text_delta(event: &RuntimeEvent) -> Option<&str> {
    let delta = event.payload.get("delta")?;
    (delta.get("kind").and_then(Value::as_str) == Some("text_delta"))
        .then(|| delta.get("text").and_then(Value::as_str))
        .flatten()
        .filter(|text| !text.is_empty())
}

pub(crate) fn user_event_transcript_item(event: &RuntimeEvent) -> Option<TranscriptItem> {
    event
        .payload
        .get("payload")
        .and_then(|payload| payload.get("prompt"))
        .or_else(|| event.payload.get("prompt"))
        .and_then(Value::as_str)
        .filter(|prompt| !prompt.trim().is_empty())
        .map(|prompt| TranscriptItem {
            role: TranscriptRole::User,
            title: "You".to_owned(),
            body: vec![prompt.to_owned()],
        })
}

pub(crate) fn assistant_event_transcript_item(event: &RuntimeEvent) -> Option<TranscriptItem> {
    event
        .payload
        .get("content")
        .and_then(Value::as_str)
        .filter(|content| !content.trim().is_empty())
        .map(|content| TranscriptItem {
            role: TranscriptRole::Assistant,
            title: "Golutra".to_owned(),
            body: vec![content.to_owned()],
        })
}

pub(crate) fn status_event_transcript_item(event: &RuntimeEvent) -> Option<TranscriptItem> {
    if event.event_type == RuntimeEventType::ProviderTransportFallback
        || event.event_type == RuntimeEventType::RetryScheduled
            && event.payload.get("recovery").is_some()
    {
        return None;
    }
    if event.event_type == RuntimeEventType::ApprovalRequested {
        // 审批走独立对话框，不在 transcript 再铺一张卡。
        return None;
    }
    if event.event_type == RuntimeEventType::ToolCompleted {
        return tool_event_transcript_item(event);
    }
    if event.event_type == RuntimeEventType::TaskCompleted
        && let Some(status) = event_task_status(event)
    {
        return terminal::status_item(
            status,
            failure_event_error(event).unwrap_or("Task finished"),
        );
    }
    let title = event_status_title(event.event_type)?;
    let summary = event_summary(event)?;
    if event.event_type == RuntimeEventType::LoopDecided {
        return terminal::loop_failed(event).then(|| TranscriptItem {
            role: TranscriptRole::Error,
            title: "Task failed".to_owned(),
            body: vec![
                visible_failure_detail(failure_event_error(event).unwrap_or(&summary)).to_owned(),
            ],
        });
    }
    Some(TranscriptItem {
        role: TranscriptRole::Status,
        title: title.to_owned(),
        body: vec![summary],
    })
}

fn event_task_status(event: &RuntimeEvent) -> Option<TaskStatus> {
    serde_json::from_value(event.payload.get("status")?.clone()).ok()
}

// Keep argument rejection visible in collapsed and restored history, where
// a generic summary alone gives neither the user nor the operator a cause.
fn invalid_tool_request_diagnostic(event: &RuntimeEvent) -> Option<String> {
    let envelope = event.payload.get("envelope")?;
    if tool_result_status(event) != ToolResultStatus::Error
        || envelope.get("summary").and_then(Value::as_str) != Some("tool request is invalid")
    {
        return None;
    }
    let tool_name = envelope
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("tool");
    let reason = envelope
        .pointer("/structured_facts/error")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            envelope
                .get("model_visible_excerpt")
                .and_then(Value::as_str)
        })?;
    let reason = reason
        .strip_prefix("tool arguments are invalid: ")
        .unwrap_or(reason);
    let prefix = format!("tool `{tool_name}` arguments do not match its contract: ");
    Some(reason.strip_prefix(&prefix).unwrap_or(reason).to_owned())
}

fn tool_event_transcript_item(event: &RuntimeEvent) -> Option<TranscriptItem> {
    let summary = event_summary(event).unwrap_or_else(|| "tool completed".to_owned());
    let status = tool_result_status(event);
    let file_changes = operation_file_changes(event);
    if !file_changes.is_empty() {
        return Some(file_change_item(&file_changes, status));
    }

    let envelope = event.payload.get("envelope");
    let tool_name = envelope
        .and_then(|value| value.get("tool_name"))
        .and_then(Value::as_str);
    if let Some(diagnostic) = invalid_tool_request_diagnostic(event) {
        return Some(TranscriptItem {
            role: tool_status_role(status),
            title: format!("Failed · {}", tool_name.unwrap_or("tool")),
            body: bounded_output_lines(&diagnostic, 3),
        });
    }
    let facts = envelope.and_then(|value| value.get("structured_facts"));
    match tool_name {
        Some("shell") => {
            let command = facts
                .and_then(|value| value.get("command"))
                .and_then(Value::as_str)
                .unwrap_or(&summary);
            Some(TranscriptItem {
                role: tool_status_role(status),
                title: completed_tool_title_with_object("shell", status, Some(command)),
                body: vec![command.to_owned()],
            })
        }
        Some("read_file" | "list_dir" | "rg_search" | "symbol_search" | "find_references") => {
            let resource = tool_resource(facts).unwrap_or(summary);
            Some(TranscriptItem {
                role: tool_status_role(status),
                title: completed_tool_title_with_object(
                    tool_name.unwrap_or("tool"),
                    status,
                    Some(&resource),
                ),
                body: vec![resource],
            })
        }
        _ => Some(TranscriptItem {
            role: tool_status_role(status),
            title: completed_tool_title(tool_name.unwrap_or("tool"), status),
            body: vec![summary],
        }),
    }
}

fn tool_result_status(event: &RuntimeEvent) -> ToolResultStatus {
    let Some(value) = event
        .payload
        .pointer("/envelope/status")
        .or_else(|| event.payload.get("status"))
    else {
        return ToolResultStatus::Ok;
    };
    serde_json::from_value(value.clone()).unwrap_or(ToolResultStatus::Error)
}

fn tool_resource(facts: Option<&Value>) -> Option<String> {
    let facts = facts?;
    for key in ["path", "query", "pattern", "symbol"] {
        if let Some(value) = facts.get(key).and_then(Value::as_str) {
            return Some(value.to_owned());
        }
    }
    None
}

fn file_change_item(changes: &[FileChangeSummary], status: ToolResultStatus) -> TranscriptItem {
    let stats_complete = changes
        .iter()
        .all(|change| change.added_lines.is_some() && change.removed_lines.is_some());
    let added = changes
        .iter()
        .filter_map(|change| change.added_lines)
        .fold(0_u64, u64::saturating_add);
    let removed = changes
        .iter()
        .filter_map(|change| change.removed_lines)
        .fold(0_u64, u64::saturating_add);
    let noun = if changes.len() == 1 { "file" } else { "files" };
    let edit_summary = if let [change] = changes {
        let verb = match change.kind {
            FileChangeKind::Added => "Created",
            FileChangeKind::Modified => "Edited",
            FileChangeKind::Deleted => "Deleted",
        };
        format!("{verb} {}", change.path)
    } else {
        format!("Edited {} {noun}", changes.len())
    };
    let title = if status == ToolResultStatus::Ok {
        edit_summary
    } else {
        format!("{} · {edit_summary}", completed_tool_title("tool", status))
    };
    let visible = changes.iter().take(if changes.len() == 1 { 0 } else { 5 });
    let mut body: Vec<String> = visible
        .map(|change| match (change.added_lines, change.removed_lines) {
            (Some(added), Some(removed)) => {
                format!("{}  +{added} -{removed}", change.path)
            }
            _ => change.path.clone(),
        })
        .collect();
    if stats_complete {
        body.insert(0, format!("└ (+{added} -{removed})"));
    }
    if changes.len() > 5 {
        body.push(format!("… {} more files", changes.len() - 5));
    }
    TranscriptItem {
        role: tool_status_role(status),
        title,
        body,
    }
}

pub(crate) fn event_status_title(event_type: RuntimeEventType) -> Option<&'static str> {
    match event_type {
        RuntimeEventType::TaskCompleted => Some("Task Completed"),
        RuntimeEventType::CommandRejected => Some("Command Rejected"),
        RuntimeEventType::ControllerChanged => Some("Controller Changed"),
        RuntimeEventType::LoopDecided => Some("Loop Decided"),
        RuntimeEventType::RetryScheduled => Some("Retrying"),
        RuntimeEventType::ProviderFallback => Some("Fallback"),
        RuntimeEventType::ProviderTransportFallback => Some("Transport Fallback"),
        RuntimeEventType::LoopGuardTriggered => Some("Stopped"),
        RuntimeEventType::TaskPaused => Some("Paused"),
        RuntimeEventType::TaskResumed => Some("Resumed"),
        RuntimeEventType::TaskAbortRequested => Some("Stopping"),
        RuntimeEventType::TaskAborted => Some("Aborted"),
        RuntimeEventType::TaskInterrupted => Some("Task Interrupted"),
        RuntimeEventType::TaskUncertain => Some("Task Uncertain / reconciliation required"),
        RuntimeEventType::TaskReconciled => Some("Task Recovery Reconciled"),
        RuntimeEventType::ToolProgress => None,
        _ => None,
    }
}

pub(crate) fn event_summary(event: &RuntimeEvent) -> Option<String> {
    event
        .payload
        .get("summary")
        .and_then(Value::as_str)
        .map_or_else(
            || {
                event
                    .payload
                    .get("error")
                    .and_then(Value::as_str)
                    .map(|error| {
                        if error.trim().is_empty() {
                            "runtime event recorded".to_owned()
                        } else {
                            error.to_owned()
                        }
                    })
            },
            |summary| {
                if summary.trim().is_empty() {
                    None
                } else {
                    Some(summary.to_owned())
                }
            },
        )
}

pub(crate) fn projection_items(projection: &UserProjection) -> Vec<TranscriptItem> {
    let mut items = projection
        .visible_steps
        .iter()
        .filter(|step| significant_step(step))
        .filter(|step| {
            !(step.label == "TaskCompleted"
                && step.status == "Failed"
                && projection
                    .visible_steps
                    .iter()
                    .any(|other| other.label == "LoopDecided" && significant_step(other)))
        })
        .map(step_item)
        .collect::<Vec<_>>();
    if let Some(pending_approval) = &projection.pending_approval {
        items.push(TranscriptItem {
            role: TranscriptRole::Status,
            title: "Approval required".to_owned(),
            body: vec![pending_approval.to_owned()],
        });
    }
    if let Some(final_message) = &projection.final_message {
        items.push(TranscriptItem {
            role: TranscriptRole::Assistant,
            title: "Golutra".to_owned(),
            body: vec![final_message.to_owned()],
        });
    }
    items
}

// Failure summaries are intentionally compact. The error field keeps the
// complete, already sanitized diagnostic and must also win during replay.
fn failure_event_error(event: &RuntimeEvent) -> Option<&str> {
    ["error", "summary"].into_iter().find_map(|key| {
        event
            .payload
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
    })
}

fn visible_failure_detail(mut text: &str) -> &str {
    // 仅移除已知错误类型的包装，保留完整上游原因；类型细分不应泄漏冗余前缀。
    loop {
        let Some(detail) = [
            "runtime task execution failed: ",
            "provider call failed: ",
            "provider failed: ",
            "provider connection failed: ",
            "provider response is malformed: ",
            "provider is temporarily unavailable: ",
        ]
        .into_iter()
        .find_map(|prefix| text.strip_prefix(prefix)) else {
            break;
        };
        text = detail;
    }
    // Genai also uses StreamParse for response.failed business errors. Present
    // its actual cause without claiming every such error is a JSON parse error.
    if text.starts_with("Failed to parse stream data for model '")
        && let Some((_, cause)) = text.split_once("Cause: ")
        && !cause.trim().is_empty()
    {
        return cause;
    }
    text
}

fn provider_failure_details(event: &RuntimeEvent) -> Vec<String> {
    let Some(metadata) = event.payload.get("error_metadata") else {
        return Vec::new();
    };
    [
        ("response_http_status", "HTTP response"),
        ("http_status", "Error status"),
        ("provider_code", "Provider code"),
        ("request_id", "Request ID"),
    ]
    .into_iter()
    .filter_map(|(key, label)| {
        let value = metadata.get(key).filter(|value| !value.is_null())?;
        let text = value
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| value.to_string());
        Some(format!("{label}: {text}"))
    })
    .collect()
}

pub(crate) fn significant_step(step: &VisibleStep) -> bool {
    matches!(step.label.as_str(), "ToolCompleted" | "CommandRejected")
        || (step.label == "TaskCompleted"
            && !matches!(step.status.as_str(), "Completed" | "Partial"))
        || (step.label == "LoopDecided"
            && step.summary.starts_with("runtime task execution failed:"))
}

pub(crate) fn step_item(step: &VisibleStep) -> TranscriptItem {
    if step.label == "TaskCompleted"
        && let Ok(status) =
            serde_json::from_value::<TaskStatus>(Value::String(step.status.to_ascii_lowercase()))
        && let Some(item) = terminal::status_item(status, &step.summary)
    {
        return item;
    }
    if step.label == "LoopDecided" && significant_step(step) {
        return TranscriptItem {
            role: TranscriptRole::Error,
            title: "Task failed".to_owned(),
            body: vec![visible_failure_detail(&step.summary).to_owned()],
        };
    }
    let role = if step.status.eq_ignore_ascii_case("failed")
        || step.summary.to_ascii_lowercase().contains("error")
    {
        TranscriptRole::Error
    } else {
        TranscriptRole::Status
    };
    TranscriptItem {
        role,
        title: readable_step_label(&step.label),
        body: vec![format!("{} - {}", step.status, step.summary)],
    }
}

pub(crate) fn readable_step_label(label: &str) -> String {
    label
        .chars()
        .enumerate()
        .fold(String::new(), |mut output, (index, character)| {
            if index > 0 && character.is_uppercase() {
                output.push(' ');
            }
            output.push(character);
            output
        })
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use golutra_agent_core::{
        EventId, SessionId, TaskId, ToolCallId, UserStep, UserStepId, UserStepKind, UserStepTool,
    };
    use golutra_agent_protocol::RuntimeEventSource;
    use serde_json::json;

    use super::*;

    #[test]
    fn command_preview_retains_the_first_and_last_output_lines() {
        for count in [0_usize, 5, 6, 10, 11, 30] {
            let mut details = vec!["cargo test".to_owned(), "Output".to_owned()];
            details.extend((0..count).map(|i| format!("output-{i}")));
            let preview = default_tool_preview(&details, true, false);
            let output = preview
                .iter()
                .filter(|line| line.starts_with("│ "))
                .cloned()
                .collect::<Vec<_>>();
            let expected = (0..count)
                .filter(|i| *i < 5 || *i >= count.saturating_sub(5))
                .map(|i| format!("│ output-{i}"))
                .collect::<Vec<_>>();
            assert_eq!(output, expected);
            if count > 10 {
                assert!(
                    preview
                        .iter()
                        .any(|line| line.contains(&format!("{} more lines", count - 10)))
                );
            }
            let running = default_tool_preview(&details, true, true);
            assert_eq!(
                running
                    .iter()
                    .filter(|line| line.starts_with("│ "))
                    .cloned()
                    .collect::<Vec<_>>(),
                (count.saturating_sub(5)..count)
                    .map(|i| format!("│ output-{i}"))
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn tool_projection_keeps_full_details_out_of_the_default_summary() {
        let projection = OperationProjection::ToolActivity {
            id: OperationId("tool-1".to_owned()),
            item: TranscriptItem {
                role: TranscriptRole::Success,
                title: "ran".to_owned(),
                body: Vec::new(),
            },
            details: vec![
                "python3 - <<'PY'".to_owned(),
                "Arguments".to_owned(),
                "{\"secret\":\"never-in-summary\"}".to_owned(),
                "Output".to_owned(),
                "hello".to_owned(),
                "second line".to_owned(),
            ],
        };

        let summary = projection.item(false);
        assert!(summary.body.iter().any(|line| line.contains("python3")));
        assert!(summary.body.iter().any(|line| line.contains("hello")));
        assert!(!summary.body.iter().any(|line| line == "Arguments"));
        assert!(
            !summary
                .body
                .iter()
                .any(|line| line.contains("never-in-summary"))
        );

        let details = projection.item(true);
        assert!(details.body.iter().any(|line| line == "Arguments"));
        assert!(
            details
                .body
                .iter()
                .any(|line| line.contains("never-in-summary"))
        );
    }

    #[test]
    fn provider_recovery_events_have_distinct_user_facing_labels() {
        assert_eq!(
            event_status_title(RuntimeEventType::ProviderFallback),
            Some("Fallback")
        );
        assert_eq!(
            event_status_title(RuntimeEventType::ProviderTransportFallback),
            Some("Transport Fallback")
        );
        assert_eq!(
            event_status_title(RuntimeEventType::TaskInterrupted),
            Some("Task Interrupted")
        );
        assert_eq!(
            event_status_title(RuntimeEventType::TaskUncertain),
            Some("Task Uncertain / reconciliation required")
        );
    }

    fn tool_event(sequence_no: u64, event_type: RuntimeEventType, payload: Value) -> RuntimeEvent {
        tool_event_on_turn(sequence_no, None, event_type, payload)
    }

    #[test]
    fn incremental_stream_projection_matches_replay_across_semantic_boundaries() {
        let turn = TurnId::new();
        let mut app = TuiApp::new(
            golutra_agent_core::ThreadId::new(),
            SessionId::new(),
            None,
            false,
            "mock".into(),
            None,
        );
        app.transcript.frame_cache_enabled = true;
        let script = [
            (
                RuntimeEventType::ProviderStreamed,
                json!({"delta":{"kind":"text_delta","text":"你好 "}}),
            ),
            (
                RuntimeEventType::ProviderStreamed,
                json!({"delta":{"kind":"text_delta","text":"世界\n\n"}}),
            ),
            (
                RuntimeEventType::ProviderStreamed,
                json!({"delta":{"kind":"reasoning_delta","text":"hidden"}}),
            ),
            (
                RuntimeEventType::RetryScheduled,
                json!({"recovery":{"reset_stream":true}}),
            ),
            (
                RuntimeEventType::ProviderStreamed,
                json!({"delta":{"kind":"text_delta","text":"重新开始 "}}),
            ),
            (
                RuntimeEventType::ProviderStreamed,
                json!({"delta":{"kind":"text_delta","text":"👩‍💻 é"}}),
            ),
            (
                RuntimeEventType::AssistantMessage,
                json!({"content":"最终修订"}),
            ),
            (
                RuntimeEventType::ProviderStreamed,
                json!({"delta":{"kind":"text_delta","text":"新片段"}}),
            ),
            (RuntimeEventType::TaskCompleted, json!({})),
        ];
        for (index, (kind, payload)) in script.into_iter().enumerate() {
            let mut event = tool_event_on_turn(index as u64 + 1, Some(turn), kind, payload);
            event.session_id = app.session_id;
            event.task_id = None;
            app.apply_runtime_event(event);
            if index == 1 || index == 5 {
                assert!(
                    app.transcript.frame_operations.borrow().is_some(),
                    "ordinary deltas should advance the cached projection"
                );
            }
            assert_eq!(
                history_event_operations(&app),
                event_operation_entries(&app.events),
                "event {index}"
            );
            // 归档前缀和光标布局变化不改变事件语义，不能强制回放所有历史。
            app.transcript
                .set_committed_stream_lines(HashMap::from([(app.events[0].id, index)]));
        }
        app.replace_event_history(app.events[3..].to_vec(), true);
        assert_eq!(
            history_event_operations(&app),
            event_operation_entries(&app.events)
        );
    }

    #[test]
    fn long_stream_reuses_projection_between_observation_events_and_matches_replay() {
        let turn = TurnId::new();
        let mut app = fullscreen_app();
        app.transcript.frame_cache_enabled = true;
        for sequence in 1..=200 {
            let mut event = tool_event_on_turn(
                sequence,
                Some(TurnId::new()),
                RuntimeEventType::AssistantMessage,
                json!({"content": format!("历史段落 {sequence}。\n\n```rust\nlet n = {sequence};\n```\n")}),
            );
            event.session_id = app.session_id;
            event.task_id = None;
            app.apply_runtime_event(event);
        }
        let mut expected = String::new();
        for index in 0..1200 {
            let (kind, payload) = if index % 3 == 1 {
                (
                    RuntimeEventType::TokenUsageRecorded,
                    json!({"input_tokens": index}),
                )
            } else if index % 3 == 2 {
                (
                    RuntimeEventType::VerificationCompleted,
                    json!({"summary":"internal checks"}),
                )
            } else {
                let delta = "流式中文 👩‍💻 é\n";
                expected.push_str(delta);
                (
                    RuntimeEventType::ProviderStreamed,
                    json!({"delta":{"kind":"text_delta","text":delta}}),
                )
            };
            let mut event = tool_event_on_turn(index + 201, Some(turn), kind, payload);
            event.session_id = app.session_id;
            event.task_id = None;
            app.apply_runtime_event(event);
            if index > 0 {
                assert!(
                    app.transcript.frame_operations.borrow().is_some(),
                    "cache invalidated at {index}"
                );
            }
            if index == 0 || index % 100 == 0 || index == 1199 {
                assert_eq!(
                    history_event_operations(&app),
                    event_operation_entries(&app.events)
                );
            }
        }
        let entries = history_event_operations(&app);
        assert_eq!(entries.len(), 201);
        assert_eq!(
            entries.last().unwrap().projection.item(false).body,
            vec![expected]
        );
        let mut foreign = tool_event_on_turn(
            1401,
            Some(turn),
            RuntimeEventType::ProviderStreamed,
            json!({"delta":{"kind":"text_delta","text":"different task"}}),
        );
        foreign.session_id = app.session_id;
        app.apply_runtime_event(foreign);
        assert!(
            app.transcript.frame_operations.borrow().is_none(),
            "task boundary must rebuild"
        );
    }

    #[test]
    fn terminal_completion_survives_a_late_running_tool_result() {
        let call = ToolCallId::new();
        let process = format!("proc-{call}");
        let ended = tool_event(
            1,
            RuntimeEventType::ProcessUpdated,
            json!({"process_id":process,"command":"sleep 1","process_state":"exited","terminal":true,"exit_code":0,"output_excerpt":"done"}),
        );
        let delayed = tool_event(
            2,
            RuntimeEventType::ToolCompleted,
            json!({"envelope":{"tool_call_id":call,"tool_name":"shell","status":"ok","structured_facts":{"process_id":process,"command":"sleep 1","process_state":"running"}}}),
        );
        let entries = event_operation_entries(&[ended, delayed]);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].projection.item(false).role,
            TranscriptRole::Success
        );
        assert!(
            entries[0]
                .projection
                .item(true)
                .body
                .iter()
                .any(|line| line == "done")
        );
    }

    #[test]
    fn subagent_completion_survives_late_start_and_archival_without_duplicate_notices() {
        let call = ToolCallId::new();
        let child = SessionId::new();
        let ended = tool_event(
            1,
            RuntimeEventType::SubagentUpdated,
            json!({"tool_call_id":call,"summary":"done","content":"child findings","facts":{"child_session_id":child,"child_status":"completed"}}),
        );
        let start = tool_event(
            2,
            RuntimeEventType::ToolCompleted,
            json!({"envelope":{"tool_call_id":call,"tool_name":"subagent","status":"ok","structured_facts":{"child_session_id":child,"child_status":"running","completed":false}}}),
        );
        let entries = event_operation_entries(&[ended.clone(), start.clone()]);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].projection.item(false).title,
            "Subagent completed"
        );
        let mut ended = ended;
        ended.sequence_no = 3;
        let mut repeated = ended.clone();
        repeated.sequence_no = 4;
        repeated.id = EventId::new();
        let archived = HashSet::from([start.id]);
        let entries = event_operation_entries_with_boundary(
            &[start, ended, repeated],
            &archived,
            &HashSet::new(),
            true,
            &HashSet::new(),
        );
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[1].projection.item(false).title,
            "Subagent completed"
        );
    }

    #[test]
    fn batch_child_wait_displays_finished_and_pending_counts() {
        let event = tool_event(
            1,
            RuntimeEventType::ToolCompleted,
            json!({"envelope":{"tool_call_id":ToolCallId::new(),"tool_name":"subagent","status":"ok","structured_facts":{"child_results":[{},{}],"child_pending_ids":["one"]}}}),
        );
        let entries = event_operation_entries(&[event]);
        assert_eq!(
            entries[0].projection.item(false).title,
            "Subagents · 1 finished · 1 running"
        );
    }

    #[test]
    fn completed_foreground_and_output_reads_have_distinct_titles() {
        let call = ToolCallId::new();
        let process = format!("proc-{call}");
        let reading = tool_event(
            0,
            RuntimeEventType::ToolStarted,
            json!({
                "tool_call_id":ToolCallId::new(),"tool_name":"shell_session",
                "arguments":{"action":"read","process_id":process}
            }),
        );
        assert_eq!(
            tool_started_projection(&reading).unwrap().item(false).title,
            "Reading terminal output"
        );
        let completed = tool_event(
            1,
            RuntimeEventType::ToolCompleted,
            json!({
                "envelope":{"tool_call_id":call,"tool_name":"shell","status":"ok",
                    "structured_facts":{"process_id":process,"command":"ls","process_state":"exited","terminal":true}}
            }),
        );
        let updated = tool_event(
            2,
            RuntimeEventType::ProcessUpdated,
            json!({
                "process_id":process,"command":"ls","process_state":"exited","terminal":true,"exit_code":0
            }),
        );
        let entries = event_operation_entries(&[completed, updated]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].projection.item(false).title, "ran");
        for state in ["running", "exited"] {
            let read = tool_event(
                3,
                RuntimeEventType::ToolCompleted,
                json!({
                    "envelope":{"tool_call_id":ToolCallId::new(),"tool_name":"shell_session","status":"ok",
                        "structured_facts":{"process_id":process,"command":"ls","action":"read","process_state":state}}
                }),
            );
            let entries = event_operation_entries(&[read]);
            assert_eq!(
                entries[0].projection.item(false).title,
                "Read terminal output"
            );
            assert_eq!(
                entries[0].projection.item(false).role,
                TranscriptRole::Success
            );
        }
    }

    #[test]
    fn terminal_updates_refresh_details_and_append_only_one_completion_after_archival() {
        let call = ToolCallId::new();
        let process = format!("proc-{call}");
        let start = tool_event(
            1,
            RuntimeEventType::ToolCompleted,
            json!({
                "envelope":{"tool_call_id":call,"tool_name":"shell","status":"ok","structured_facts":{"process_id":process,"command":"curl example.test","process_state":"running","workspace_changes_known":false},"summary":"running"}
            }),
        );
        let running = tool_event(
            2,
            RuntimeEventType::ProcessUpdated,
            json!({"process_id":process,"command":"curl example.test","process_state":"running","terminal":false,"elapsed_ms":50,"output_excerpt":"live output"}),
        );
        let ended = tool_event(
            3,
            RuntimeEventType::ProcessUpdated,
            json!({"process_id":process,"command":"curl example.test","process_state":"exited","terminal":true,"exit_code":0,"elapsed_ms":100,"output_excerpt":"final output"}),
        );
        let mut settled = ended.clone();
        settled.id = EventId::new();
        settled.sequence_no = 4;
        settled.payload["workspace_scan_pending"] = json!(false);
        let archived = HashSet::from([start.id]);
        let entries = event_operation_entries_with_boundary(
            &[start, running, ended, settled],
            &archived,
            &HashSet::new(),
            true,
            &HashSet::new(),
        );
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[1].projection.item(false).title,
            "Background terminal completed"
        );
        assert!(
            entries[0]
                .projection
                .item(true)
                .body
                .iter()
                .any(|line| line.contains("final output"))
        );
        assert!(
            !entries[0]
                .projection
                .item(false)
                .body
                .iter()
                .any(|line| line.contains("workspace changes unknown"))
        );
    }

    #[test]
    fn consecutive_terminal_waits_merge_without_crossing_messages_or_failures() {
        let turn = TurnId::new();
        let wait = |seq, state: &str| {
            tool_event_on_turn(
                seq,
                Some(turn),
                RuntimeEventType::ToolCompleted,
                json!({"envelope":{"tool_call_id":ToolCallId::new(),"tool_name":"shell_session","status":"ok","summary":"wait","structured_facts":{"action":"wait","process_id":"one","command":"sleep 1","process_state":state}}}),
            )
        };
        let message = tool_event_on_turn(
            3,
            Some(turn),
            RuntimeEventType::AssistantMessage,
            json!({"content":"Still working."}),
        );
        let entries = event_operation_entries_with_boundary(
            &[
                wait(1, "running"),
                wait(2, "running"),
                message,
                wait(4, "exited"),
            ],
            &HashSet::new(),
            &HashSet::new(),
            true,
            &HashSet::new(),
        );
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].event_ids.len(), 2);
        assert!(entries[1].projection.is_assistant_message());
    }

    fn fullscreen_app() -> TuiApp {
        let mut app = TuiApp::new(
            golutra_agent_core::ThreadId::new(),
            SessionId::new(),
            None,
            false,
            "ready (mock)".into(),
            None,
        );
        app.transcript.fullscreen = true;
        app
    }

    #[test]
    fn fullscreen_groups_exploration_but_keeps_execution_and_failure_boundaries() {
        let turn = TurnId::new();
        let mut app = fullscreen_app();
        app.events.extend(completed_read(1, turn, "a.txt"));
        let first_id = history_event_operations(&app)[0]
            .projection
            .id()
            .cloned()
            .unwrap();
        app.transcript.toggle_operation(first_id.clone());
        app.events.extend(completed_read(3, turn, "b.txt"));
        for (sequence, name, status) in [
            (5, "shell", "ok"),
            (8, "write_file", "ok"),
            (9, "shell", "error"),
        ] {
            app.events.push(tool_event_on_turn(sequence, Some(turn), RuntimeEventType::ToolCompleted,
                json!({"envelope":{"tool_call_id":ToolCallId::new(), "tool_name":name, "status":status,
                    "summary":"actual failure", "structured_facts":{"command":"cargo test", "path":"out.txt"}}})));
            if sequence == 5 {
                app.events.extend(completed_read(6, turn, "c.txt"));
            }
        }
        let entries = history_event_operations(&app);
        assert_eq!(entries.len(), 5);
        assert_eq!(entries[0].projection.item(false).title, "read 2 files");
        assert_eq!(entries[0].projection.id(), Some(&first_id));
        assert!(app.transcript.is_expanded(entries[0].projection.id()));
        assert_eq!(entries[1].tool_kind, ToolSummaryKind::Ran);
        assert_eq!(entries[2].tool_kind, ToolSummaryKind::Read);
        assert_eq!(entries[3].tool_kind, ToolSummaryKind::Edited);
        assert_eq!(
            entries[4].projection.item(false).role,
            TranscriptRole::Error
        );
        assert!(
            entries[4]
                .projection
                .item(false)
                .body
                .iter()
                .any(|line| line.contains("actual failure"))
        );
        assert!(
            entries[0]
                .projection
                .item(true)
                .body
                .iter()
                .any(|line| line.contains("Arguments"))
        );
    }

    #[test]
    fn fullscreen_status_anchors_at_complete_blocks_and_preserves_revised_final() {
        let turn = TurnId::new();
        let mut app = fullscreen_app();
        app.events
            .push(assistant_delta(1, turn, "完整段落。\n\n尚未"));
        app.record_slash_command("/status");
        app.push_system_message("Status", vec!["Running".into()]);
        app.events
            .push(assistant_delta(2, turn, "完成的段落。\n\n最后一段。"));
        let items = transcript_items(&app);
        let text = items
            .iter()
            .map(|item| item.body.join("\n"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.find("完整段落。").unwrap() < text.find("/status").unwrap());
        assert!(text.find("/status").unwrap() < text.find("尚未完成的段落。").unwrap());
        assert_eq!(text.matches("Running").count(), 1);
        app.events.push(tool_event_on_turn(
            3,
            Some(turn),
            RuntimeEventType::AssistantMessage,
            json!({"content":"最终修订后的完整回答。"}),
        ));
        let text = transcript_items(&app)
            .iter()
            .map(|item| item.body.join("\n"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(text.matches("最终修订后的完整回答。").count(), 1);
        assert_eq!(text.matches("/status").count(), 1);
    }

    #[test]
    fn individual_tool_can_collapse_after_expand_all() {
        let id = OperationId("test".into());
        let mut state = TranscriptState::default();
        state.toggle_details();
        assert!(state.is_expanded(Some(&id)));
        state.toggle_operation(id.clone());
        assert!(!state.is_expanded(Some(&id)));
        state.toggle_operation(id.clone());
        assert!(state.is_expanded(Some(&id)));
        state.toggle_details();
        assert!(!state.is_expanded(Some(&id)));
    }

    #[test]
    fn opened_running_tool_keeps_its_identity_when_it_completes() {
        let mut app = fullscreen_app();
        let turn = TurnId::new();
        app.events.extend(completed_read(1, turn, "a.txt"));
        let mut second = completed_read(3, turn, "b.txt");
        let completion = second.pop().unwrap();
        app.events.extend(second);
        let id = history_event_operations(&app)[1]
            .projection
            .id()
            .cloned()
            .unwrap();
        app.transcript.toggle_operation(id.clone());
        app.events.push(completion);
        let entries = history_event_operations(&app);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].projection.id(), Some(&id));
        assert!(app.transcript.is_expanded(Some(&id)));
    }

    #[test]
    fn inline_tool_detail_keeps_running_identity_and_hidden_draft_on_completion() {
        let mut app = fullscreen_app();
        app.transcript.fullscreen = false;
        app.transcript.compact_tools = true;
        app.enable_inline_history();
        app.input.insert_str("保留中文草稿🙂");
        let turn = TurnId::new();
        app.events.extend(completed_read(1, turn, "a.txt"));
        let mut second = completed_read(3, turn, "b.txt");
        let completion = second.pop().unwrap();
        app.events.extend(second);
        let id = history_event_operations(&app)[1]
            .projection
            .id()
            .cloned()
            .unwrap();
        crate::tool_detail::open_tool_detail(&mut app, id.clone());
        crate::handle_paste("不能修改草稿", &mut app);
        app.events.push(completion);
        let mut terminal =
            crate::managed_terminal::Terminal::new(ratatui::backend::TestBackend::new(80, 24))
                .unwrap();
        terminal
            .draw(|frame| crate::draw_ui(frame, &mut app))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("b.txt"), "{text}");
        assert!(
            !text.contains("a.txt"),
            "selected tool must not be swallowed by its predecessor"
        );
        assert!(
            !text.contains("Reading"),
            "completion must update the open detail"
        );
        assert!(
            app.transcript.expanded_operations.is_empty(),
            "opening details must not expand inline history"
        );
        assert_eq!(history_event_operations(&app)[1].projection.id(), Some(&id));
        crate::tool_detail::close_tool_detail(&mut app);
        assert_eq!(app.input.text(), "保留中文草稿🙂");
        assert!(app.transcript.history.enabled);
        assert!(!app.transcript.fullscreen);
    }

    #[test]
    fn tool_detail_scrolls_failed_output_and_survives_tiny_viewports() {
        let mut app = fullscreen_app();
        app.transcript.fullscreen = false;
        app.transcript.compact_tools = true;
        let output = (0..80)
            .map(|n| format!("输出{n:03}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.events.push(tool_event(1, RuntimeEventType::ToolCompleted, json!({
            "envelope": {"tool_call_id": ToolCallId::new(), "tool_name":"shell", "status":"error",
                "summary":"exit 2: syntax error", "structured_facts":{"command":"cargo test", "exit_code":2},
                "model_visible_excerpt":output}
        })));
        let id = history_event_operations(&app)[0]
            .projection
            .id()
            .cloned()
            .unwrap();
        crate::tool_detail::open_tool_detail(&mut app, id);
        let mut terminal =
            crate::managed_terminal::Terminal::new(ratatui::backend::TestBackend::new(60, 12))
                .unwrap();
        terminal
            .draw(|frame| crate::draw_ui(frame, &mut app))
            .unwrap();
        let first = terminal.backend().buffer().clone();
        assert!(
            first
                .content
                .iter()
                .map(|c| c.symbol())
                .collect::<String>()
                .contains("Failed")
        );
        crate::tool_detail::handle_tool_detail_key(
            crate::KeyEvent::new(crate::KeyCode::End, crate::KeyModifiers::NONE),
            &mut app,
        );
        terminal
            .draw(|frame| crate::draw_ui(frame, &mut app))
            .unwrap();
        assert_ne!(terminal.backend().buffer(), &first);
        for (width, height) in [(1, 1), (8, 2), (60, 12)] {
            terminal.backend_mut().resize(width, height);
            terminal
                .resize(ratatui::layout::Rect::new(0, 0, width, height))
                .unwrap();
            terminal
                .draw(|frame| crate::draw_ui(frame, &mut app))
                .unwrap();
        }
        crate::tool_detail::handle_tool_detail_key(
            crate::KeyEvent::new(crate::KeyCode::Esc, crate::KeyModifiers::NONE),
            &mut app,
        );
        assert!(app.tool_detail.is_none());
    }

    #[test]
    fn fullscreen_summary_bounds_long_commands_without_hiding_failure_or_details() {
        let mut app = fullscreen_app();
        let command = "echo 中文参数 ".repeat(200);
        app.events.push(tool_event_on_turn(1, Some(TurnId::new()), RuntimeEventType::ToolCompleted,
            json!({"envelope":{"tool_call_id":ToolCallId::new(), "tool_name":"shell", "status":"error",
                "summary":"exit 2: syntax error", "structured_facts":{"command":command}}})));
        let area = ratatui::layout::Rect::new(0, 0, 32, 24);
        let collapsed = crate::transcript_layout(&app, area);
        assert!(collapsed.row_count < 20);
        assert!(collapsed.plain_text().contains("Failed"));
        assert!(collapsed.plain_text().contains("syntax error"));
        app.transcript.toggle_details();
        let expanded = crate::transcript_layout(&app, area);
        assert!(expanded.plain_text().contains(&command));
        assert!(expanded.row_count > collapsed.row_count);
    }

    #[test]
    fn fullscreen_observation_history_remains_scrollable() {
        let mut app = fullscreen_app();
        app.debug_mode = true;
        let turn = TurnId::new();
        for n in 0..100 {
            app.events.push(assistant_delta(
                n + 1,
                turn,
                &format!("观察内容{n:03}。\n\n"),
            ));
        }
        let mut terminal =
            crate::managed_terminal::Terminal::new(ratatui::backend::TestBackend::new(100, 24))
                .unwrap();
        terminal
            .draw(|frame| crate::draw_ui(frame, &mut app))
            .unwrap();
        let tail = terminal.backend().buffer().clone();
        app.scroll_active_pane(golutra_agent_tui::TranscriptScrollAction::Top);
        terminal
            .draw(|frame| crate::draw_ui(frame, &mut app))
            .unwrap();
        assert!(app.debug_scroll.offset_from_bottom > 0);
        assert_ne!(terminal.backend().buffer(), &tail);
        app.scroll_active_pane(golutra_agent_tui::TranscriptScrollAction::Bottom);
        terminal
            .draw(|frame| crate::draw_ui(frame, &mut app))
            .unwrap();
        assert_eq!(terminal.backend().buffer(), &tail);
    }

    fn tool_event_on_turn(
        sequence_no: u64,
        turn_id: Option<TurnId>,
        event_type: RuntimeEventType,
        payload: Value,
    ) -> RuntimeEvent {
        RuntimeEvent {
            schema_version: golutra_agent_core::RUNTIME_EVENT_SCHEMA_VERSION,
            causal_context: Default::default(),
            causal_links: Vec::new(),
            id: EventId::new(),
            sequence_no,
            session_id: SessionId::new(),
            turn_id,
            task_id: Some(TaskId::new()),
            parent_event_id: None,
            event_type,
            timestamp: Utc::now(),
            source: RuntimeEventSource::Tool,
            payload,
            payload_ref: None,
            durable: true,
        }
    }

    #[test]
    fn file_tool_events_have_a_compact_change_summary() {
        let event = RuntimeEvent {
            schema_version: golutra_agent_core::RUNTIME_EVENT_SCHEMA_VERSION,
            causal_context: Default::default(),
            causal_links: Vec::new(),
            id: EventId::new(),
            sequence_no: 1,
            session_id: SessionId::new(),
            turn_id: None,
            task_id: Some(TaskId::new()),
            parent_event_id: None,
            event_type: RuntimeEventType::ToolCompleted,
            timestamp: Utc::now(),
            source: RuntimeEventSource::Tool,
            payload: json!({
                "summary": "file edited",
                "file_changes": [{
                    "path": "src/lib.rs",
                    "kind": "modified",
                    "added_lines": 3,
                    "removed_lines": 1
                }]
            }),
            payload_ref: None,
            durable: true,
        };

        let item = status_event_transcript_item(&event).expect("change item");

        assert_eq!(item.title, "Edited src/lib.rs");
        assert_eq!(item.body, ["└ (+3 -1)"]);
    }

    #[test]
    fn legacy_changed_files_remain_visible_without_fake_line_counts() {
        let event = RuntimeEvent {
            schema_version: golutra_agent_core::RUNTIME_EVENT_SCHEMA_VERSION,
            causal_context: Default::default(),
            causal_links: Vec::new(),
            id: EventId::new(),
            sequence_no: 1,
            session_id: SessionId::new(),
            turn_id: None,
            task_id: Some(TaskId::new()),
            parent_event_id: None,
            event_type: RuntimeEventType::ToolCompleted,
            timestamp: Utc::now(),
            source: RuntimeEventSource::Tool,
            payload: json!({
                "summary": "file edited",
                "changed_files": ["src/legacy.rs"]
            }),
            payload_ref: None,
            durable: true,
        };

        let item = status_event_transcript_item(&event).expect("legacy change item");

        assert_eq!(item.title, "Edited src/legacy.rs");
        assert!(item.body.is_empty());
    }

    #[test]
    fn tool_lifecycle_is_projected_as_one_expandable_operation() {
        let tool_call_id = ToolCallId::new();
        let events = vec![
            tool_event(
                1,
                RuntimeEventType::ToolStarted,
                json!({
                    "tool_call_id": tool_call_id,
                    "tool_name": "shell",
                    "arguments": {"command": "cargo test"}
                }),
            ),
            tool_event(
                2,
                RuntimeEventType::ToolProgress,
                json!({
                    "tool_call_id": tool_call_id,
                    "tool_name": "shell",
                    "progress": {
                        "phase": "output",
                        "elapsed_ms": 120,
                        "output_bytes": 42,
                        "output_lines": 3,
                        "detail": "stdout",
                        "output_excerpt": "running test one\nrunning test two"
                    }
                }),
            ),
            tool_event(
                3,
                RuntimeEventType::ToolCompleted,
                json!({
                    "envelope": {
                        "tool_call_id": tool_call_id,
                        "tool_name": "shell",
                        "status": "ok",
                        "summary": "shell command completed",
                        "structured_facts": {"command": "cargo test"},
                        "model_visible_excerpt": "one\ntwo\nthree\nfour"
                    },
                    "metrics": {
                        "duration_ms": 240,
                        "output_bytes": 42,
                        "output_lines": 4,
                        "output_truncated": false,
                        "exit_code": 0
                    }
                }),
            ),
        ];

        let running = event_operation_projections(&events[..2]);
        let OperationProjection::ToolActivity { .. } = &running[0] else {
            panic!("running tool projection");
        };
        let running_expanded = running[0].item(true);
        assert!(
            running_expanded
                .body
                .iter()
                .any(|line| line.contains("stdout · 3 lines"))
        );
        assert!(
            running_expanded
                .body
                .iter()
                .any(|line| line == "│ running test two")
        );

        let projections = event_operation_projections(&events);

        assert_eq!(projections.len(), 1);
        let OperationProjection::ToolActivity { item, details, .. } = &projections[0] else {
            panic!("tool lifecycle should remain a tool operation");
        };
        assert_eq!(item.title, "ran");
        assert_eq!(item.role, TranscriptRole::Success);
        let collapsed = projections[0].item(false);
        assert!(collapsed.body.iter().any(|line| line == "  └ cargo test"));
        assert!(collapsed.body.iter().any(|line| line == "│ one"));
        assert!(
            projections[0]
                .item(true)
                .body
                .iter()
                .any(|line| line == "one")
        );
        assert!(details.iter().any(|line| line == "cargo test"));
        assert!(details.iter().any(|line| line.contains("exit 0")));
        assert!(details.iter().all(|line| line != "Facts"));
        assert!(details.iter().any(|line| line == "four"));
        assert_eq!(running[0].item(false).title, "Running");
        assert_eq!(
            running[0]
                .item(false)
                .body
                .iter()
                .filter(|line| line.contains("running test two"))
                .count(),
            1
        );
        assert!(
            running[0]
                .item(false)
                .body
                .iter()
                .any(|line| line.contains("running test two"))
        );
    }

    #[test]
    fn terminal_tool_statuses_have_distinct_user_visible_roles() {
        let cases = [
            ("ok", "ran", TranscriptRole::Success),
            ("error", "Failed", TranscriptRole::Error),
            ("timeout", "Timed out", TranscriptRole::Warning),
            ("blocked", "Blocked", TranscriptRole::Warning),
            ("cancelled", "Cancelled", TranscriptRole::System),
        ];

        for (status, title, role) in cases {
            let projections = event_operation_projections(&[tool_event(
                1,
                RuntimeEventType::ToolCompleted,
                json!({
                    "envelope": {
                        "tool_call_id": ToolCallId::new(),
                        "tool_name": "shell",
                        "status": status,
                        "summary": "terminal result",
                        "structured_facts": {}
                    }
                }),
            )]);
            let OperationProjection::ToolActivity { item, .. } = &projections[0] else {
                panic!("terminal tool result should be an activity");
            };
            assert_eq!(item.title, title);
            assert_eq!(item.role, role);
        }
    }

    #[test]
    fn file_diff_preview_is_visible_in_both_card_and_details() {
        let event = tool_event(
            1,
            RuntimeEventType::ToolCompleted,
            json!({
                "envelope": {
                    "tool_call_id": ToolCallId::new(),
                    "tool_name": "edit_file",
                    "status": "ok",
                    "summary": "file edited",
                    "structured_facts": {}
                },
                "file_changes": [{
                    "path": "src/lib.rs",
                    "kind": "modified",
                    "added_lines": 1,
                    "removed_lines": 1
                }],
                "diff_previews": [{
                    "path": "src/lib.rs",
                    "lines": ["-old", "+new"],
                    "truncated": false
                }]
            }),
        );
        let projection = event_operation_projections(&[event])[0].clone();
        let collapsed = projection.item(false);
        let expanded = projection.item(true);

        assert!(
            !collapsed
                .body
                .iter()
                .any(|line| line.contains("src/lib.rs"))
        );
        assert!(expanded.body.iter().any(|line| line == "-old"));
        assert!(expanded.body.iter().any(|line| line == "+new"));
        assert!(collapsed.body.iter().any(|line| line == "-old"));
        assert!(collapsed.body.iter().any(|line| line == "+new"));
    }

    #[test]
    fn created_and_deleted_cards_use_actual_line_counts() {
        for (kind, added, removed, title) in [
            ("added", 1, 0, "Created test.py"),
            ("deleted", 0, 2, "Deleted test.py"),
        ] {
            let event = tool_event(
                1,
                RuntimeEventType::ToolCompleted,
                json!({"file_changes":[{"path":"test.py","kind":kind,"added_lines":added,"removed_lines":removed}]}),
            );
            assert_eq!(status_event_transcript_item(&event).unwrap().title, title);
        }
    }

    #[test]
    fn failed_shell_that_changed_files_keeps_failure_and_execution_context() {
        let event = tool_event(
            1,
            RuntimeEventType::ToolCompleted,
            json!({
                "envelope": {
                    "tool_call_id": ToolCallId::new(),
                    "tool_name": "shell",
                    "status": "error",
                    "summary": "shell command failed",
                    "structured_facts": {
                        "command": "printf new > src/lib.rs; false",
                        "exit_code": 1,
                        "workspace_changes_known": true
                    },
                    "model_visible_excerpt": "command failure output"
                },
                "metrics": {
                    "duration_ms": 125,
                    "output_bytes": 22,
                    "output_lines": 1,
                    "output_truncated": false,
                    "exit_code": 1
                },
                "file_changes": [{
                    "path": "src/lib.rs",
                    "kind": "modified",
                    "added_lines": 1,
                    "removed_lines": 0
                }],
                "diff_previews": [{
                    "path": "src/lib.rs",
                    "lines": ["+new"],
                    "truncated": false
                }]
            }),
        );

        let projection = event_operation_projections(&[event])[0].clone();
        let OperationProjection::FileChange { item, details, .. } = projection else {
            panic!("file-changing shell should remain a file change operation");
        };

        assert_eq!(item.role, TranscriptRole::Error);
        assert_eq!(item.title, "Failed · Edited src/lib.rs");
        assert!(item.body.iter().any(|line| line == "shell command failed"));
        assert!(item.title.contains("src/lib.rs"));
        assert!(
            details
                .iter()
                .any(|line| line == "printf new > src/lib.rs; false")
        );
        assert!(details.iter().any(|line| line.contains("exit 1")));
        assert!(details.iter().any(|line| line == "Output"));
        assert!(details.iter().any(|line| line == "command failure output"));
        assert!(details.iter().all(|line| line != "Facts"));
        assert!(details.iter().any(|line| line == "+new"));
    }

    #[test]
    fn legacy_file_change_payload_uses_its_terminal_status() {
        let event = tool_event(
            1,
            RuntimeEventType::ToolCompleted,
            json!({
                "status": "timeout",
                "summary": "command timed out",
                "file_changes": [{
                    "path": "partial.txt",
                    "kind": "added"
                }]
            }),
        );

        let item = status_event_transcript_item(&event).expect("legacy change item");

        assert_eq!(item.role, TranscriptRole::Warning);
        assert_eq!(item.title, "Timed out · Created partial.txt");
    }

    #[test]
    fn malformed_terminal_status_never_projects_as_success() {
        let projection = event_operation_projections(&[tool_event(
            1,
            RuntimeEventType::ToolCompleted,
            json!({
                "envelope": {
                    "tool_call_id": ToolCallId::new(),
                    "tool_name": "shell",
                    "status": "unexpected",
                    "summary": "malformed terminal result",
                    "structured_facts": {}
                }
            }),
        )])[0]
            .clone();
        let item = projection.item(false);

        assert_eq!(item.role, TranscriptRole::Error);
        assert_eq!(item.title, "Failed");
    }

    #[test]
    fn invalid_tool_arguments_show_the_cause_in_collapsed_and_restored_history() {
        let cause = "max_output_bytes: 20000 is greater than the maximum of 4096";
        let diagnostic = format!(
            "tool arguments are invalid: tool `shell` arguments do not match its contract: {cause}"
        );
        for facts in [json!({"error":diagnostic}), json!({})] {
            let event = tool_event(
                1,
                RuntimeEventType::ToolCompleted,
                json!({
                    "envelope": {
                        "tool_call_id": ToolCallId::new(), "tool_name":"shell", "status":"error",
                        "summary":"tool request is invalid", "structured_facts":facts,
                        "model_visible_excerpt":diagnostic
                    }
                }),
            );
            let projection = event_operation_projections(std::slice::from_ref(&event)).remove(0);
            for item in [
                projection.item(false),
                projection.item(true),
                tool_event_transcript_item(&event).unwrap(),
            ] {
                assert_eq!(item.role, TranscriptRole::Error);
                assert_eq!(item.title, "Failed · shell");
                assert!(item.body.iter().any(|line| line == cause), "{item:?}");
            }
            let OperationProjection::ToolActivity { details, .. } = projection else {
                panic!("tool activity")
            };
            assert!(details.contains(&diagnostic));
        }
    }

    #[test]
    fn opaque_tool_side_effects_are_explicitly_labeled_unknown() {
        let projection = event_operation_projections(&[tool_event(
            1,
            RuntimeEventType::ToolCompleted,
            json!({
                "envelope": {
                    "tool_call_id": ToolCallId::new(),
                    "tool_name": "mcp__fixture__echo",
                    "status": "ok",
                    "summary": "external call completed",
                    "structured_facts": {"workspace_changes_known": false}
                }
            }),
        )])[0]
            .clone();
        let collapsed = projection.item(false);
        let expanded = projection.item(true);

        assert!(
            collapsed
                .body
                .iter()
                .any(|line| line.contains("workspace changes unknown"))
        );
        assert!(
            expanded
                .body
                .iter()
                .any(|line| line == "workspace changes unknown")
        );
    }

    fn assistant_delta(sequence_no: u64, turn_id: TurnId, text: &str) -> RuntimeEvent {
        tool_event_on_turn(
            sequence_no,
            Some(turn_id),
            RuntimeEventType::ProviderStreamed,
            json!({"delta": {"kind": "text_delta", "text": text}}),
        )
    }

    #[test]
    fn repeated_retry_previews_are_hidden_until_confirmed_and_new_requests_stream_normally() {
        let turn = TurnId::new();
        let mut events = vec![assistant_delta(1, turn, "first partial")];
        events[0].payload["provider_request_id"] = json!("request-one");
        for index in 0..3 {
            events.push(tool_event_on_turn(
                2 + index * 2,
                Some(turn),
                RuntimeEventType::RetryScheduled,
                json!({"provider_request_id":"request-one", "recovery":{"reset_stream":true}}),
            ));
            let mut preview = assistant_delta(3 + index * 2, turn, "repeated partial");
            preview.payload["provider_request_id"] = json!("request-one");
            events.push(preview);
        }
        let pending = event_operation_projections(&events);
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].item(false).body,
            vec!["first partial", "[Response interrupted; retrying]"]
        );
        events.push(tool_event_on_turn(
            8,
            Some(turn),
            RuntimeEventType::ProviderCompleted,
            json!({"provider_request_id":"request-one"}),
        ));
        events.push(tool_event_on_turn(
            9,
            Some(turn),
            RuntimeEventType::AssistantMessage,
            json!({"content":"complete answer"}),
        ));
        let mut next = assistant_delta(10, turn, "next request");
        next.payload["provider_request_id"] = json!("request-two");
        events.push(next);
        let replayed = event_operation_projections(&events);
        assert_eq!(replayed.len(), 3);
        assert_eq!(replayed[1].item(false).body, vec!["complete answer"]);
        assert_eq!(replayed[2].item(false).body, vec!["next request"]);
    }

    #[test]
    fn recovery_boundary_never_joins_partial_text_to_the_next_attempt() {
        let turn = TurnId::new();
        let events = vec![
            assistant_delta(1, turn, "旧的中文半句"),
            tool_event_on_turn(
                2,
                Some(turn),
                RuntimeEventType::RetryScheduled,
                json!({"recovery": {"phase": "waiting", "reset_stream": true}}),
            ),
            tool_event_on_turn(
                3,
                Some(turn),
                RuntimeEventType::RetryScheduled,
                json!({"recovery": {"phase": "retrying", "reset_stream": false}}),
            ),
            assistant_delta(4, turn, "新的完整"),
            assistant_delta(5, turn, "回答。"),
            tool_event_on_turn(
                6,
                Some(turn),
                RuntimeEventType::AssistantMessage,
                json!({"content": "新的完整回答。"}),
            ),
        ];
        let items = event_operation_projections(&events)
            .into_iter()
            .map(|item| item.item(false))
            .collect::<Vec<_>>();
        assert_eq!(items.len(), 2, "retry events do not become history cards");
        assert_eq!(
            items[0].body,
            vec!["旧的中文半句", "[Response interrupted; retrying]"]
        );
        assert_eq!(items[1].body, vec!["新的完整回答。"]);
        let committed = HashSet::from([events[0].id]);
        let replayed = event_operation_entries_with_boundary(
            &events,
            &committed,
            &HashSet::new(),
            false,
            &HashSet::new(),
        );
        assert!(replayed.iter().all(|item| item.stable));
        assert_eq!(replayed.len(), 2);
    }

    fn completed_read(sequence_no: u64, turn_id: TurnId, path: &str) -> Vec<RuntimeEvent> {
        completed_read_with_id(sequence_no, turn_id, ToolCallId::new(), path)
    }

    fn completed_read_with_id(
        sequence_no: u64,
        turn_id: TurnId,
        tool_call_id: ToolCallId,
        path: &str,
    ) -> Vec<RuntimeEvent> {
        vec![
            tool_event_on_turn(
                sequence_no,
                Some(turn_id),
                RuntimeEventType::ToolStarted,
                json!({
                    "tool_call_id": tool_call_id,
                    "tool_name": "read_file",
                    "arguments": {"path": path}
                }),
            ),
            tool_event_on_turn(
                sequence_no + 1,
                Some(turn_id),
                RuntimeEventType::ToolCompleted,
                json!({
                    "envelope": {
                        "tool_call_id": tool_call_id,
                        "tool_name": "read_file",
                        "status": "ok",
                        "summary": "file read",
                        "structured_facts": {"path": path}
                    }
                }),
            ),
        ]
    }

    #[test]
    fn assistant_text_between_tool_batches_stays_as_separate_model_narration() {
        let turn_id = TurnId::new();
        let mut events = vec![assistant_delta(1, turn_id, "先摸清仓库结构。")];
        events.extend(completed_read(2, turn_id, "README.md"));
        events.extend(completed_read(4, turn_id, "Cargo.toml"));
        events.push(assistant_delta(6, turn_id, "再补架构信息。"));
        events.push(tool_event_on_turn(
            7,
            Some(turn_id),
            RuntimeEventType::AssistantMessage,
            json!({"content": "先摸清仓库结构。再补架构信息。最终回复"}),
        ));
        let projections = event_operation_projections(&events);
        let titles_and_bodies = projections
            .iter()
            .map(|projection| {
                let item = projection.item(false);
                (item.title, item.body)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            titles_and_bodies[0],
            ("Golutra".to_owned(), vec!["先摸清仓库结构。".to_owned()])
        );
        assert_eq!(titles_and_bodies[1].0, "read 2 files");
        assert!(
            titles_and_bodies[1]
                .1
                .iter()
                .any(|line| line.contains("README.md"))
        );
        assert_eq!(
            titles_and_bodies[2],
            ("Golutra".to_owned(), vec!["再补架构信息。".to_owned()])
        );
        assert_eq!(titles_and_bodies.len(), 3);
    }

    #[test]
    fn single_successful_read_keeps_the_file_name_instead_of_a_count() {
        let projections =
            event_operation_projections(&completed_read(1, TurnId::new(), "README.md"));
        let item = projections[0].item(false);
        assert_eq!(item.title, "read README.md");
    }

    #[test]
    fn failed_shell_stays_outside_the_successful_tool_summary() {
        let turn_id = TurnId::new();
        let mut events = completed_read(1, turn_id, "README.md");
        events.extend(completed_read(3, turn_id, "Cargo.toml"));
        events.push(tool_event_on_turn(
            5,
            Some(turn_id),
            RuntimeEventType::ToolCompleted,
            json!({
                "envelope": {
                    "tool_call_id": ToolCallId::new(),
                    "tool_name": "shell",
                    "status": "error",
                    "summary": "command failed",
                    "structured_facts": {"command": "false"}
                }
            }),
        ));
        let projections = event_operation_projections(&events);
        assert_eq!(projections.len(), 2);
        assert_eq!(projections[0].item(false).title, "read 2 files");
        assert_eq!(projections[1].item(false).title, "Failed");
    }

    #[test]
    fn in_progress_read_is_not_folded_into_completed_summary() {
        let turn_id = TurnId::new();
        let mut events = completed_read(1, turn_id, "README.md");
        events.extend(completed_read(3, turn_id, "Cargo.toml"));
        events.push(tool_event_on_turn(
            5,
            Some(turn_id),
            RuntimeEventType::ToolStarted,
            json!({
                "tool_call_id": ToolCallId::new(),
                "tool_name": "read_file",
                "arguments": {"path": "docs/ARCHITECTURE.md"}
            }),
        ));
        let projections = event_operation_projections(&events);
        assert_eq!(projections.len(), 2);
        assert_eq!(projections[0].item(false).title, "read 2 files");
        assert_eq!(projections[1].item(false).title, "Reading ARCHITECTURE.md");
    }

    #[test]
    fn mixed_batch_prints_user_visible_titles() {
        let turn_id = TurnId::new();
        let list_id = ToolCallId::new();
        let shell_id = ToolCallId::new();
        let mut events = vec![assistant_delta(1, turn_id, "先摸清仓库结构。")];
        events.extend(completed_read(2, turn_id, "README.md"));
        events.extend(completed_read(4, turn_id, "Cargo.toml"));
        events.push(tool_event_on_turn(
            6,
            Some(turn_id),
            RuntimeEventType::ToolStarted,
            json!({
                "tool_call_id": list_id,
                "tool_name": "list_dir",
                "arguments": {"path": "crates"}
            }),
        ));
        events.push(tool_event_on_turn(
            7,
            Some(turn_id),
            RuntimeEventType::ToolCompleted,
            json!({
                "envelope": {
                    "tool_call_id": list_id,
                    "tool_name": "list_dir",
                    "status": "ok",
                    "summary": "listed",
                    "structured_facts": {"path": "crates"}
                }
            }),
        ));
        events.push(assistant_delta(8, turn_id, "再核对入口后跑检查。"));
        events.push(tool_event_on_turn(
            9,
            Some(turn_id),
            RuntimeEventType::ToolStarted,
            json!({
                "tool_call_id": shell_id,
                "tool_name": "shell",
                "arguments": {"command": "cargo test -p golutra-agent-tui"}
            }),
        ));
        events.push(tool_event_on_turn(
            10,
            Some(turn_id),
            RuntimeEventType::ToolCompleted,
            json!({
                "envelope": {
                    "tool_call_id": shell_id,
                    "tool_name": "shell",
                    "status": "ok",
                    "summary": "ok",
                    "structured_facts": {"command": "cargo test -p golutra-agent-tui"}
                }
            }),
        ));
        events.push(tool_event_on_turn(
            11,
            Some(turn_id),
            RuntimeEventType::AssistantMessage,
            json!({"content": "先摸清仓库结构。再核对入口后跑检查。最终回复不应覆盖步间短句。"}),
        ));

        let items = event_transcript_items(&events);
        let rendered = items
            .iter()
            .map(|item| {
                if item.body.is_empty() {
                    item.title.clone()
                } else {
                    format!("{} | {}", item.title, item.body.join(" / "))
                }
            })
            .collect::<Vec<_>>();
        for line in &rendered {
            println!("VISIBLE {line}");
        }
        assert_eq!(rendered[0], "Golutra | 先摸清仓库结构。");
        assert!(rendered[1].starts_with("read 2 files, listed crates"));
        assert_eq!(rendered[2], "Golutra | 再核对入口后跑检查。");
        assert!(rendered[3].starts_with("ran"));
        assert!(rendered[3].contains("cargo test -p golutra-agent-tui"));
        assert_eq!(rendered.len(), 4);
    }

    #[test]
    fn user_step_shell_batch_keeps_the_command_preview() {
        let turn_id = TurnId::new();
        let tool_call_id = ToolCallId::new();
        let events = vec![
            tool_event_on_turn(
                1,
                Some(turn_id),
                RuntimeEventType::ToolCompleted,
                json!({
                    "envelope": {
                        "tool_call_id": tool_call_id,
                        "tool_name": "shell",
                        "status": "ok",
                        "summary": "ok",
                        "structured_facts": {"command": "git status --short"}
                    }
                }),
            ),
            tool_event_on_turn(
                2,
                Some(turn_id),
                RuntimeEventType::UserStep,
                json!({
                    "step": UserStep {
                        step_id: UserStepId::new(),
                        turn_id,
                        kind: UserStepKind::ToolBatch {
                            summary: "ran".to_owned(),
                            tools: vec![golutra_agent_core::user_step_tool_from_envelope(
                                tool_call_id,
                                "shell".to_owned(),
                                ToolResultStatus::Ok,
                                &json!({"command": "git status --short"}),
                            )],
                        },
                    }
                }),
            ),
        ];
        let item = event_transcript_items(&events)[0].clone();
        assert_eq!(item.title, "ran");
        assert!(
            item.body
                .iter()
                .any(|line| line == "  └ git status --short")
        );
    }

    #[test]
    fn user_step_summary_keeps_failure_evidence_and_expandable_output() {
        let turn_id = TurnId::new();
        let tool_call_id = ToolCallId::new();
        let events = vec![
            tool_event_on_turn(
                1,
                Some(turn_id),
                RuntimeEventType::ToolCompleted,
                json!({
                    "envelope": {"tool_call_id":tool_call_id,"tool_name":"shell","status":"error",
                    "summary":"permission denied", "structured_facts":{"command":"check"},
                    "model_visible_excerpt":"DIAGNOSTIC_ONLY_IN_DETAILS"}
                }),
            ),
            tool_event_on_turn(
                2,
                Some(turn_id),
                RuntimeEventType::UserStep,
                json!({
                    "step": UserStep { step_id: UserStepId::new(), turn_id,
                        kind: UserStepKind::ToolBatch { summary:"check failed".into(), tools: vec![
                            golutra_agent_core::user_step_tool_from_envelope(tool_call_id, "shell".into(), ToolResultStatus::Error, &json!({"command":"check"}))
                        ]}
                    }
                }),
            ),
        ];
        let projections = event_operation_projections(&events);
        assert_eq!(projections.len(), 1);
        let compact = projections[0].item(false);
        assert_eq!(compact.role, TranscriptRole::Error);
        assert!(
            compact
                .body
                .iter()
                .any(|line| line.contains("permission denied"))
        );
        assert!(
            !compact
                .body
                .iter()
                .any(|line| line.contains("DIAGNOSTIC_ONLY_IN_DETAILS"))
        );
        assert!(
            projections[0]
                .item(true)
                .body
                .iter()
                .any(|line| line.contains("DIAGNOSTIC_ONLY_IN_DETAILS"))
        );
    }

    #[test]
    fn user_step_events_render_visible_text_and_tool_batches() {
        let turn_id = TurnId::new();
        let events = vec![
            tool_event_on_turn(
                1,
                Some(turn_id),
                RuntimeEventType::UserStep,
                json!({
                    "step": UserStep {
                        step_id: UserStepId::new(),
                        turn_id,
                        kind: UserStepKind::AssistantText {
                            text: "先看仓库结构。".to_owned(),
                        },
                    }
                }),
            ),
            tool_event_on_turn(
                2,
                Some(turn_id),
                RuntimeEventType::UserStep,
                json!({
                    "step": UserStep {
                        step_id: UserStepId::new(),
                        turn_id,
                        kind: UserStepKind::ToolBatch {
                            summary: "read 2 files, listed crates".to_owned(),
                            tools: vec![
                                UserStepTool {
                                    tool_call_id: ToolCallId::new(),
                                    tool_name: "read_file".to_owned(),
                                    status: ToolResultStatus::Ok,
                                    object: Some("README.md".to_owned()),
                                },
                                UserStepTool {
                                    tool_call_id: ToolCallId::new(),
                                    tool_name: "read_file".to_owned(),
                                    status: ToolResultStatus::Ok,
                                    object: Some("Cargo.toml".to_owned()),
                                },
                                UserStepTool {
                                    tool_call_id: ToolCallId::new(),
                                    tool_name: "list_dir".to_owned(),
                                    status: ToolResultStatus::Ok,
                                    object: Some("crates".to_owned()),
                                },
                            ],
                        },
                    }
                }),
            ),
            tool_event_on_turn(
                3,
                Some(turn_id),
                RuntimeEventType::AssistantMessage,
                json!({"content": "最终整段回复不应覆盖 UserStep 短句。"}),
            ),
        ];
        let items = event_transcript_items(&events);
        assert_eq!(items[0].title, "Golutra");
        assert_eq!(items[0].body, vec!["先看仓库结构。".to_owned()]);
        assert_eq!(items[1].title, "read 2 files, listed crates");
        assert!(items.iter().all(|item| {
            item.body
                .iter()
                .all(|line| line != "最终整段回复不应覆盖 UserStep 短句。")
        }));
    }

    #[test]
    fn user_step_path_keeps_user_prompt_and_interleaves_live_tools() {
        let turn_id = TurnId::new();
        let read_id = ToolCallId::new();
        let mut events = vec![tool_event_on_turn(
            1,
            Some(turn_id),
            RuntimeEventType::TaskCreated,
            json!({"payload": {"prompt": "看仓库结构"}}),
        )];
        events.push(assistant_delta(2, turn_id, "先看仓库结构。"));
        events.extend(completed_read_with_id(3, turn_id, read_id, "README.md"));
        events.push(tool_event_on_turn(
            5,
            Some(turn_id),
            RuntimeEventType::UserStep,
            json!({
                "step": UserStep {
                    step_id: UserStepId::new(),
                    turn_id,
                    kind: UserStepKind::AssistantText {
                        text: "先看仓库结构。".to_owned(),
                    },
                }
            }),
        ));
        events.push(tool_event_on_turn(
            6,
            Some(turn_id),
            RuntimeEventType::UserStep,
            json!({
                "step": UserStep {
                    step_id: UserStepId::new(),
                    turn_id,
                    kind: UserStepKind::ToolBatch {
                        summary: "read README.md".to_owned(),
                        tools: vec![UserStepTool {
                            tool_call_id: read_id,
                            tool_name: "read_file".to_owned(),
                            status: ToolResultStatus::Ok,
                            object: Some("README.md".to_owned()),
                        }],
                    },
                }
            }),
        ));
        events.push(tool_event_on_turn(
            7,
            Some(turn_id),
            RuntimeEventType::ToolStarted,
            json!({
                "tool_call_id": ToolCallId::new(),
                "tool_name": "list_dir",
                "arguments": {"path": "crates"}
            }),
        ));
        events.push(tool_event_on_turn(
            8,
            Some(turn_id),
            RuntimeEventType::AssistantMessage,
            json!({"content": "先看仓库结构。最终整段回复不应覆盖短句。"}),
        ));

        let items = event_transcript_items(&events);
        let rendered = items
            .iter()
            .map(|item| {
                if item.body.is_empty() {
                    item.title.clone()
                } else {
                    format!("{} | {}", item.title, item.body.join(" / "))
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(rendered[0], "You | 看仓库结构");
        assert_eq!(rendered[1], "Golutra | 先看仓库结构。");
        assert!(rendered[2].starts_with("read README.md"));
        assert!(rendered[3].starts_with("Listing crates"));
        assert_eq!(rendered.len(), 4);
    }

    #[test]
    fn background_session_and_subagent_follow_stable_titles() {
        let waited = event_operation_projections(&[tool_event(
            1,
            RuntimeEventType::ToolCompleted,
            json!({
                "envelope": {
                    "tool_call_id": ToolCallId::new(),
                    "tool_name": "shell_session",
                    "status": "ok",
                    "summary": "still running",
                    "structured_facts": {"command": "cargo test"}
                }
            }),
        )]);
        assert_eq!(
            waited[0].item(false).title,
            "Waited for background terminal"
        );
        assert!(
            waited[0]
                .item(false)
                .body
                .iter()
                .any(|line| line == "  └ cargo test")
        );

        let interacted = event_operation_projections(&[tool_event(
            1,
            RuntimeEventType::ToolCompleted,
            json!({
                "envelope": {
                    "tool_call_id": ToolCallId::new(),
                    "tool_name": "shell_session",
                    "status": "ok",
                    "summary": "sent input",
                    "structured_facts": {"command": "python", "stdin": "print(1)\n"}
                }
            }),
        )]);
        assert_eq!(
            interacted[0].item(false).title,
            "Interacted with background terminal"
        );

        let child = event_operation_projections(&[tool_event(
            1,
            RuntimeEventType::ToolCompleted,
            json!({
                "envelope": {
                    "tool_call_id": ToolCallId::new(),
                    "tool_name": "subagent",
                    "status": "ok",
                    "summary": "child completed",
                    "structured_facts": {"child_status": "completed"}
                }
            }),
        )]);
        assert_eq!(child[0].item(false).title, "subagent");
        assert!(
            child[0]
                .item(false)
                .body
                .iter()
                .any(|line| line.contains("completed"))
        );
    }

    #[test]
    fn partial_subagent_shows_verification_issue_and_preserves_expandable_findings() {
        let projections = event_operation_projections(&[tool_event(
            1,
            RuntimeEventType::ToolCompleted,
            json!({
                "envelope": {"tool_call_id": ToolCallId::new(), "tool_name": "subagent", "status": "error",
                    "summary": "subagent returned findings with unresolved verification",
                    "model_visible_excerpt": "项目名称：Golutra Agent",
                    "structured_facts": {"child_status": "partial", "child_session_id": "child-id", "workspace_changes_known": false,
                        "child_verification_status": "partial", "child_verification_issues": [{"reason": "required validation is missing"}]}}
            }),
        )]);
        let collapsed = projections[0].item(false);
        assert_eq!(collapsed.role, TranscriptRole::Warning);
        assert_eq!(collapsed.title, "subagent · verification incomplete");
        assert!(
            collapsed
                .body
                .iter()
                .any(|line| line.contains("required validation is missing"))
        );
        assert!(
            !collapsed
                .body
                .iter()
                .any(|line| line.contains("workspace changes unknown"))
        );
        let expanded = projections[0].item(true);
        assert!(
            expanded
                .body
                .iter()
                .any(|line| line.contains("项目名称：Golutra Agent"))
        );
        assert!(expanded.body.iter().any(|line| line.contains("child-id")));
        assert!(
            expanded
                .body
                .iter()
                .any(|line| line.contains("workspace changes unknown"))
        );
    }

    #[test]
    fn subagent_task_state_does_not_hide_runtime_timeout() {
        let projections = event_operation_projections(&[tool_event(
            1,
            RuntimeEventType::ToolCompleted,
            json!({
                "envelope": {"tool_call_id": ToolCallId::new(), "tool_name": "subagent", "status": "timeout",
                    "summary": "child exceeded its maximum elapsed time", "structured_facts": {"child_status": "partial", "timed_out": true}}
            }),
        )]);
        assert_eq!(projections[0].item(false).title, "Timed out");
        assert_eq!(projections[0].item(false).role, TranscriptRole::Warning);
    }
}
