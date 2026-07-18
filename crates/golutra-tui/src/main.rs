use std::{
    io::{self, Stdout},
    path::{Path, PathBuf},
    sync::LazyLock,
    time::{Duration, Instant},
};

use clap::Parser;
use crossterm::{
    cursor::SetCursorStyle,
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event as CrosstermEvent, KeyCode,
        KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use golutra_auth::{
    CredentialRef, CredentialSource, OAuthFlow, OAuthProviderDescriptor, SecretKind,
};
use golutra_client::{
    DebugExportCoordinator, DebugExportRequest, RuntimeClient, RuntimeTransport,
    parse_session_range,
};
use golutra_config::{
    BuiltinOAuthMethod, ProviderConfigPaths, ProviderConfigScope, ProviderInstallPlan,
    ProviderProfile, apply_oauth_provider_install_plan_verified,
    apply_provider_install_plan_verified, generate_custom_provider_api_key_env,
    load_provider_settings, logout_provider_profile_verified, provider_auth_service,
    provider_onboarding_state, provider_protocol_has_runtime_adapter,
    update_provider_settings_verified,
};
use golutra_core::{ActorKind, QueryId, SessionId, TaskId, ThreadId};
use golutra_llm::{
    ProviderGenerationConfig, ProviderHeaderConfig, ProviderHeaderValue, ProviderProtocol,
    provider_protocol_catalog,
};
use golutra_protocol::{
    EventFilter, EventPageDirection, EventPageRequest, RuntimeEvent, RuntimeEventType,
    RuntimeQuery, RuntimeQueryKind, SessionCommandKind, UserProjection,
};
use golutra_tui::{
    AuthConfigScope, AuthCredentialStore, OAuthLoginCommand, OpenAiCompatibleLogin,
    PaneScrollState, SlashAuthCommand, SlashCommand, SlashCommandCandidate, SlashInput,
    TranscriptScrollAction, parse_slash_input, slash_command_candidates,
};
use ratatui::{Terminal, backend::CrosstermBackend};
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

static TUI_ACTOR_ID: LazyLock<String> =
    LazyLock::new(|| format!("golutra-tui-{}-{}", std::process::id(), Uuid::now_v7()));
const TUI_HISTORY_PAGE_SIZE: u32 = 256;

mod auth_flow;
mod auth_state;
mod composer_input;
mod developer;
mod render;
mod session;
pub(crate) use auth_flow::*;
pub(crate) use auth_state::*;
pub(crate) use composer_input::*;
pub(crate) use developer::*;
pub(crate) use render::*;
pub(crate) use session::*;

#[derive(Debug, Parser)]
#[command(name = "golutra-tui")]
#[command(about = "Golutra terminal chat UI")]
struct Args {
    #[arg(long)]
    cwd: Option<std::path::PathBuf>,
    #[arg(long, conflicts_with = "connect")]
    daemon: bool,
    #[arg(long, value_name = "URL", conflicts_with = "daemon")]
    connect: Option<String>,
    #[arg(long, value_name = "UUID")]
    session_id: Option<String>,
    #[arg(long, value_name = "UUID")]
    task_id: Option<String>,
    #[arg(long)]
    debug: bool,
}

#[derive(Debug)]
struct TuiApp {
    thread_id: ThreadId,
    session_id: SessionId,
    task_id: Option<TaskId>,
    projection: Option<UserProjection>,
    developer_projection: Option<golutra_protocol::DebugProjection>,
    developer_error: Option<String>,
    events: Vec<Value>,
    command_messages: Vec<TranscriptItem>,
    resume_picker: Option<ResumePickerState>,
    export_flow: Option<ExportFlowState>,
    auth_dialog: Option<AuthDialogState>,
    auth_operation: Option<PendingAuthOperation>,
    input: ComposerInput,
    slash_selected: usize,
    status_message: String,
    provider_message: String,
    provider_model: String,
    workspace_path: PathBuf,
    debug_mode: bool,
    transcript_scroll: PaneScrollState,
    developer_scroll: PaneScrollState,
    developer_load_requested: bool,
    layout: UiLayoutSnapshot,
    cursor: Option<u64>,
    history_start_cursor: Option<u64>,
    history_has_more_before: bool,
    history_load_requested: bool,
    quit_shortcut_expires_at: Option<Instant>,
    should_quit: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TranscriptRole {
    User,
    Assistant,
    Status,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TranscriptItem {
    role: TranscriptRole,
    title: String,
    body: Vec<String>,
}

struct ProviderUiStatus {
    message: String,
    model: String,
}

impl TuiApp {
    fn new(
        thread_id: ThreadId,
        session_id: SessionId,
        task_id: Option<TaskId>,
        debug_mode: bool,
        provider_message: String,
        auth_dialog: Option<AuthDialogState>,
    ) -> Self {
        let provider_model = provider_model_from_status(&provider_message);
        let workspace_path = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            thread_id,
            session_id,
            task_id,
            projection: None,
            developer_projection: None,
            developer_error: None,
            events: Vec::new(),
            command_messages: Vec::new(),
            resume_picker: None,
            export_flow: None,
            auth_dialog,
            auth_operation: None,
            input: ComposerInput::default(),
            slash_selected: 0,
            status_message: String::new(),
            provider_message,
            provider_model,
            workspace_path,
            debug_mode,
            transcript_scroll: PaneScrollState {
                follow_tail: true,
                ..PaneScrollState::default()
            },
            developer_scroll: PaneScrollState {
                follow_tail: true,
                ..PaneScrollState::default()
            },
            developer_load_requested: false,
            layout: UiLayoutSnapshot::default(),
            cursor: None,
            history_start_cursor: None,
            history_has_more_before: false,
            history_load_requested: false,
            quit_shortcut_expires_at: None,
            should_quit: false,
        }
    }

    fn with_footer_context(
        mut self,
        workspace_path: impl Into<PathBuf>,
        provider_model: impl Into<String>,
    ) -> Self {
        self.workspace_path = workspace_path.into();
        self.provider_model = provider_model.into();
        self
    }

    fn refresh_provider_status(&mut self) {
        let status = current_provider_ui_status();
        self.provider_message = status.message;
        self.provider_model = status.model;
    }

    async fn refresh(&mut self, transport: &RuntimeTransport) -> miette::Result<()> {
        let previous_row_count = self.transcript_scroll.row_count;
        let projection = transport
            .query(RuntimeQuery {
                query_id: QueryId::new(),
                session_id: self.session_id,
                task_id: self.task_id,
                kind: RuntimeQueryKind::UserProjection,
                requester: ActorKind::Tui,
                cursor: None,
                timestamp: chrono::Utc::now(),
            })
            .await
            .map_err(|error| miette::miette!("{error}"))?;
        self.projection = serde_json::from_value(projection)
            .map(Some)
            .map_err(|error| miette::miette!("{error}"))?;
        if self.projection.as_ref().is_some_and(|projection| {
            projection.status == golutra_core::TaskStatus::WaitingAuthentication
        }) && self.auth_dialog.is_none()
            && self.auth_operation.is_none()
        {
            self.auth_dialog = Some(AuthDialogState::new());
            self.status_message = "provider authentication required".to_owned();
        }
        if self.debug_mode {
            match load_debug_projection(transport, self.session_id, self.task_id).await {
                Ok(projection) => {
                    self.developer_projection = Some(match self.developer_projection.take() {
                        Some(previous) => merge_debug_projection(previous, projection),
                        None => projection,
                    });
                    self.developer_error = None;
                }
                Err(error) => {
                    self.developer_projection = None;
                    self.developer_error = Some(error);
                }
            }
        } else {
            self.developer_projection = None;
            self.developer_error = None;
            self.developer_scroll.reset(0);
            self.developer_load_requested = false;
        }
        self.sync_transcript_row_count(previous_row_count);
        self.sync_developer_row_count();
        Ok(())
    }

    async fn load_recent_history(&mut self, transport: &RuntimeTransport) -> miette::Result<()> {
        let page = transport
            .event_page(EventPageRequest {
                session_id: self.session_id,
                task_id: self.task_id,
                cursor: None,
                direction: EventPageDirection::Backward,
                limit: TUI_HISTORY_PAGE_SIZE,
            })
            .await
            .map_err(|error| miette::miette!("{error}"))?;
        self.events = page
            .events
            .into_iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| miette::miette!("{error}"))?;
        self.history_start_cursor = page.start_cursor;
        self.cursor = page.end_cursor;
        self.history_has_more_before = page.has_more;
        self.history_load_requested = false;
        self.sync_transcript_row_count(0);
        self.clamp_transcript_scroll();
        Ok(())
    }

    async fn load_older_history(&mut self, transport: &RuntimeTransport) -> miette::Result<()> {
        self.history_load_requested = false;
        if !self.history_has_more_before {
            return Ok(());
        }
        let previous_rows = transcript_rows(self).len();
        let page = transport
            .event_page(EventPageRequest {
                session_id: self.session_id,
                task_id: self.task_id,
                cursor: self.history_start_cursor,
                direction: EventPageDirection::Backward,
                limit: TUI_HISTORY_PAGE_SIZE,
            })
            .await
            .map_err(|error| miette::miette!("{error}"))?;
        let mut older = page
            .events
            .into_iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| miette::miette!("{error}"))?;
        if older.is_empty() {
            self.history_has_more_before = false;
            return Ok(());
        }
        older.append(&mut self.events);
        self.events = older;
        self.history_start_cursor = page.start_cursor;
        self.history_has_more_before = page.has_more;
        let current_rows = transcript_rows(self).len();
        if !self.transcript_scroll.follow_tail && current_rows > previous_rows {
            self.transcript_scroll.offset_from_bottom = self
                .transcript_scroll
                .offset_from_bottom
                .saturating_add(current_rows.saturating_sub(previous_rows));
        }
        self.transcript_scroll.row_count = current_rows;
        self.clamp_transcript_scroll();
        Ok(())
    }

    async fn set_debug_mode(
        &mut self,
        transport: &RuntimeTransport,
        enabled: bool,
    ) -> miette::Result<()> {
        self.debug_mode = enabled;
        if enabled {
            self.refresh(transport).await?;
            self.status_message = "developer runtime view visible".to_owned();
        } else {
            self.developer_projection = None;
            self.developer_error = None;
            self.developer_scroll.reset(0);
            self.developer_load_requested = false;
            self.status_message = "developer runtime view hidden".to_owned();
        }
        Ok(())
    }

    fn apply_runtime_event(&mut self, event: RuntimeEvent) {
        if self
            .cursor
            .is_some_and(|sequence_no| event.sequence_no <= sequence_no)
        {
            return;
        }
        self.history_start_cursor = self.history_start_cursor.or(Some(event.sequence_no));
        self.cursor = Some(event.sequence_no);
        if let Ok(value) = serde_json::to_value(event) {
            self.events.push(value);
        }
    }

    async fn send_prompt(&mut self, transport: &RuntimeTransport) -> miette::Result<()> {
        if self.auth_operation.is_some() {
            self.status_message = "finish or cancel the auth operation first".to_owned();
            self.input.clear();
            return Ok(());
        }
        if self.auth_dialog.is_some() {
            self.status_message = "finish provider setup first".to_owned();
            self.input.clear();
            return Ok(());
        }
        if self.resume_picker.is_some() || self.export_flow.is_some() {
            self.status_message = "select a session with arrow keys or Esc".to_owned();
            self.input.clear();
            return Ok(());
        }

        let input = self.input.trimmed();
        match parse_slash_input(&input) {
            SlashInput::Prompt(prompt) => self.send_runtime_prompt(transport, prompt).await,
            SlashInput::Command(command) => {
                self.input.clear();
                self.execute_slash_command(transport, command).await
            }
            SlashInput::Empty => {
                self.status_message = "prompt is empty".to_owned();
                Ok(())
            }
            SlashInput::Error(error) => {
                self.input.clear();
                self.push_system_message("Command error", vec![error]);
                Ok(())
            }
        }
    }

    async fn accept_slash_candidate(
        &mut self,
        transport: &RuntimeTransport,
    ) -> miette::Result<bool> {
        let candidates = self.slash_candidates();
        let Some(candidate) = candidates
            .get(self.slash_selected.min(candidates.len().saturating_sub(1)))
            .cloned()
        else {
            return Ok(false);
        };
        let trimmed_input = self.input.text().trim();
        if trimmed_input.starts_with(&format!("{} ", candidate.command)) {
            return Ok(false);
        }
        if candidate.execute_on_select {
            self.input.set_text(candidate.command);
            self.send_prompt(transport).await?;
        } else {
            self.input.set_text(format!("{} ", candidate.command));
            self.slash_selected = 0;
            self.status_message = "complete slash command arguments".to_owned();
        }
        Ok(true)
    }

    fn slash_candidates(&self) -> Vec<SlashCommandCandidate> {
        if self.auth_operation.is_some()
            || self.auth_dialog.is_some()
            || self.resume_picker.is_some()
            || self.export_flow.is_some()
        {
            return Vec::new();
        }
        slash_command_candidates(self.input.text())
    }

    fn move_slash_selection(&mut self, direction: ResumeSelectionDirection) -> bool {
        let candidates = self.slash_candidates();
        if candidates.is_empty() {
            self.slash_selected = 0;
            return false;
        }
        self.slash_selected = self.slash_selected.min(candidates.len().saturating_sub(1));
        self.slash_selected = match direction {
            ResumeSelectionDirection::Previous => self.slash_selected.saturating_sub(1),
            ResumeSelectionDirection::Next => {
                (self.slash_selected + 1).min(candidates.len().saturating_sub(1))
            }
        };
        true
    }

    fn reset_slash_selection(&mut self) {
        self.slash_selected = 0;
    }

    fn reset_transcript_view(&mut self) {
        self.transcript_scroll.reset(transcript_rows(self).len());
    }

    fn reset_history_window(&mut self) {
        self.cursor = None;
        self.history_start_cursor = None;
        self.history_has_more_before = false;
        self.history_load_requested = false;
    }

    fn sync_transcript_row_count(&mut self, previous_row_count: usize) {
        let current_row_count = transcript_rows(self).len();
        if !self.transcript_scroll.follow_tail && current_row_count > previous_row_count {
            self.transcript_scroll.offset_from_bottom = self
                .transcript_scroll
                .offset_from_bottom
                .saturating_add(current_row_count - previous_row_count);
        }
        self.transcript_scroll.row_count = current_row_count;
        self.clamp_transcript_scroll();
    }

    fn scroll_transcript(&mut self, action: TranscriptScrollAction, visible_rows: usize) {
        if self.auth_dialog.is_some() || self.resume_picker.is_some() {
            return;
        }
        self.transcript_scroll.scroll(action, visible_rows);
        if matches!(
            action,
            TranscriptScrollAction::LineDown
                | TranscriptScrollAction::PageDown
                | TranscriptScrollAction::Bottom
        ) {
            self.history_load_requested = false;
        } else if self.history_has_more_before
            && matches!(
                action,
                TranscriptScrollAction::LineUp
                    | TranscriptScrollAction::PageUp
                    | TranscriptScrollAction::Top
            )
            && self.transcript_scroll.offset_from_bottom
                == self.max_transcript_scroll_offset(visible_rows)
        {
            self.history_load_requested = true;
        }
        self.status_message = transcript_scroll_status(self.transcript_scroll.offset_from_bottom);
    }

    fn clamp_transcript_scroll(&mut self) {
        self.transcript_scroll.clamp(usize::MAX);
    }

    fn max_transcript_scroll_offset(&self, visible_rows: usize) -> usize {
        self.transcript_scroll.max_offset(visible_rows)
    }

    fn sync_developer_row_count(&mut self) {
        let row_count = self
            .developer_projection
            .as_ref()
            .map(|projection| {
                developer_panel_rows(projection, usize::MAX)
                    .into_iter()
                    .filter(|row| matches!(row, DeveloperPanelRow::Event { .. }))
                    .count()
            })
            .unwrap_or(0);
        self.developer_scroll.set_row_count(row_count);
    }

    fn scroll_developer(&mut self, action: TranscriptScrollAction, visible_rows: usize) {
        if self.auth_dialog.is_some() || self.resume_picker.is_some() {
            return;
        }
        self.developer_scroll.scroll(action, visible_rows);
        if self.developer_scroll.offset_from_bottom
            == self.developer_scroll.max_offset(visible_rows)
            && self
                .developer_projection
                .as_ref()
                .is_some_and(|projection| projection.event_window.has_more_before)
            && matches!(
                action,
                TranscriptScrollAction::LineUp
                    | TranscriptScrollAction::PageUp
                    | TranscriptScrollAction::Top
            )
        {
            self.developer_load_requested = true;
        }
        self.status_message = developer_scroll_status(self.developer_scroll.offset_from_bottom);
    }

    async fn load_older_debug_history(
        &mut self,
        transport: &RuntimeTransport,
    ) -> miette::Result<()> {
        self.developer_load_requested = false;
        let Some(projection) = &mut self.developer_projection else {
            return Ok(());
        };
        let previous_rows = self.developer_scroll.row_count;
        load_older_debug_events(transport, projection)
            .await
            .map_err(|error| miette::miette!("{error}"))?;
        let current_rows = developer_panel_rows(projection, usize::MAX)
            .into_iter()
            .filter(|row| matches!(row, DeveloperPanelRow::Event { .. }))
            .count();
        if !self.developer_scroll.follow_tail && current_rows > previous_rows {
            self.developer_scroll.offset_from_bottom = self
                .developer_scroll
                .offset_from_bottom
                .saturating_add(current_rows - previous_rows);
        }
        self.developer_scroll.row_count = current_rows;
        self.developer_scroll.clamp(usize::MAX);
        Ok(())
    }

    async fn send_runtime_prompt(
        &mut self,
        transport: &RuntimeTransport,
        prompt: String,
    ) -> miette::Result<()> {
        if prompt.trim().is_empty() {
            self.status_message = "prompt is empty".to_owned();
            return Ok(());
        }

        let ack = transport
            .send_command(session_command(
                self.session_id,
                SessionCommandKind::Prompt,
                json!({
                    "prompt": prompt,
                    "_thread_id": self.thread_id.to_string(),
                }),
            ))
            .await
            .map_err(|error| miette::miette!("{error}"))?;
        self.input.clear();
        self.status_message = compact_ack_reason(&ack.reason);
        self.reset_transcript_view();
        self.refresh(transport).await
    }

    async fn execute_slash_command(
        &mut self,
        transport: &RuntimeTransport,
        command: SlashCommand,
    ) -> miette::Result<()> {
        if has_active_task(self)
            && matches!(
                &command,
                SlashCommand::New | SlashCommand::Resume { .. } | SlashCommand::Fork { .. }
            )
        {
            self.status_message = "interrupt the active task before switching sessions".to_owned();
            return Ok(());
        }
        match command {
            SlashCommand::Auth(SlashAuthCommand::Setup) => {
                self.open_auth_dialog();
            }
            SlashCommand::Help => {
                self.push_system_message("Slash commands", slash_help_lines());
            }
            SlashCommand::New => {
                self.start_new_session();
            }
            SlashCommand::Auth(command) => {
                self.execute_auth_command(transport, command).await?;
            }
            SlashCommand::Resume { thread_id } => {
                if let Some(thread_id) = thread_id {
                    self.resume_thread(transport, parse_thread_id(&thread_id)?)
                        .await?;
                } else {
                    self.open_resume_picker(transport).await?;
                }
            }
            SlashCommand::Export => {
                self.open_export_flow(transport).await?;
            }
            SlashCommand::Threads { limit } => {
                let threads = transport
                    .list_threads(limit)
                    .await
                    .map_err(|error| miette::miette!("{error}"))?;
                let lines = if threads.is_empty() {
                    vec!["no threads in this cwd yet".to_owned()]
                } else {
                    threads
                        .into_iter()
                        .map(|thread| {
                            format!(
                                "{}  {}",
                                short_id(&thread.thread_id.to_string()),
                                thread.title
                            )
                        })
                        .collect()
                };
                self.push_system_message("Threads", lines);
            }
            SlashCommand::Fork {
                thread_id,
                from_turn_id,
            } => {
                let thread = transport
                    .fork_thread(
                        parse_thread_id(&thread_id)?,
                        from_turn_id.as_deref().map(parse_turn_id).transpose()?,
                    )
                    .await
                    .map_err(|error| miette::miette!("{error}"))?;
                self.thread_id = thread.thread_id;
                self.session_id = thread.session_id;
                self.task_id = None;
                self.projection = None;
                self.developer_projection = None;
                self.developer_error = None;
                self.developer_scroll.reset(0);
                self.developer_load_requested = false;
                self.events.clear();
                self.command_messages.clear();
                self.input.clear();
                self.export_flow = None;
                self.reset_slash_selection();
                self.reset_history_window();
                self.reset_transcript_view();
                self.status_message =
                    format!("forked thread {}", short_id(&thread.thread_id.to_string()));
                self.push_system_message(
                    "Thread forked",
                    vec![
                        format!("thread {}", thread.thread_id),
                        format!(
                            "parent {}",
                            thread
                                .parent_thread_id
                                .map(|id| id.to_string())
                                .unwrap_or_else(|| "none".to_owned())
                        ),
                        format!("session {}", thread.session_id),
                    ],
                );
                self.refresh(transport).await?;
            }
            SlashCommand::Status => {
                self.refresh(transport).await?;
                let status = self
                    .projection
                    .as_ref()
                    .map(|projection| format!("{:?}", projection.status))
                    .unwrap_or_else(|| "loading".to_owned());
                self.push_system_message(
                    "Status",
                    vec![
                        format!("thread {}", self.thread_id),
                        format!("session {}", self.session_id),
                        format!(
                            "task {}",
                            self.task_id
                                .map(|id| id.to_string())
                                .unwrap_or_else(|| "auto".to_owned())
                        ),
                        format!("status {status}"),
                        format!("events {}", self.events.len()),
                    ],
                );
            }
            SlashCommand::Debug => {
                self.set_debug_mode(transport, !self.debug_mode).await?;
            }
            SlashCommand::Takeover => {
                self.send_control_command(transport, SessionCommandKind::Takeover)
                    .await?;
            }
            SlashCommand::Abort => {
                self.abort(transport).await?;
            }
            SlashCommand::Pause => {
                self.send_control_command(transport, SessionCommandKind::Pause)
                    .await?;
            }
            SlashCommand::Continue => {
                self.send_control_command(transport, SessionCommandKind::Resume)
                    .await?;
            }
            SlashCommand::Approve => {
                self.resolve_pending_approval(transport, SessionCommandKind::Approve)
                    .await?;
            }
            SlashCommand::Deny => {
                self.resolve_pending_approval(transport, SessionCommandKind::Deny)
                    .await?;
            }
            SlashCommand::Compact => {
                self.send_control_command(transport, SessionCommandKind::Compact)
                    .await?;
            }
            SlashCommand::Clear => {
                self.command_messages.clear();
                self.reset_transcript_view();
                self.status_message = "local command messages cleared".to_owned();
            }
            SlashCommand::Quit => {
                self.should_quit = true;
            }
        }
        Ok(())
    }

    async fn open_resume_picker(&mut self, transport: &RuntimeTransport) -> miette::Result<()> {
        let threads = transport
            .list_threads(50)
            .await
            .map_err(|error| miette::miette!("{error}"))?;
        let items = threads
            .into_iter()
            .map(|thread| ResumeThreadItem {
                thread_id: thread.thread_id,
                session_id: thread.session_id,
                title: thread.title,
                preview: thread.preview,
            })
            .collect::<Vec<_>>();

        if items.is_empty() {
            self.push_system_message("Resume", vec!["no sessions in this cwd yet".to_owned()]);
            return Ok(());
        }

        self.input.clear();
        self.resume_picker = Some(ResumePickerState { items, selected: 0 });
        self.status_message = "select a session to resume".to_owned();
        Ok(())
    }

    async fn open_export_flow(&mut self, transport: &RuntimeTransport) -> miette::Result<()> {
        let threads = transport
            .list_threads(50)
            .await
            .map_err(|error| miette::miette!("{error}"))?;
        let items = threads
            .into_iter()
            .map(|thread| ResumeThreadItem {
                thread_id: thread.thread_id,
                session_id: thread.session_id,
                title: thread.title,
                preview: thread.preview,
            })
            .collect::<Vec<_>>();
        if items.is_empty() {
            self.push_system_message("Export", vec!["no sessions in this cwd yet".to_owned()]);
            return Ok(());
        }
        self.input.clear();
        self.resume_picker = None;
        self.export_flow = Some(ExportFlowState {
            picker: ResumePickerState { items, selected: 0 },
            step: ExportFlowStep::SelectSession,
            range_input: "1".to_owned(),
            destination_input: String::new(),
            error: None,
            receipt: None,
        });
        self.status_message = "select a session to export".to_owned();
        Ok(())
    }

    fn close_export_flow(&mut self) {
        self.export_flow = None;
        self.status_message = "export cancelled".to_owned();
    }

    fn export_input_mut(&mut self) -> Option<&mut String> {
        let flow = self.export_flow.as_mut()?;
        match flow.step {
            ExportFlowStep::Range => Some(&mut flow.range_input),
            ExportFlowStep::Destination => Some(&mut flow.destination_input),
            _ => None,
        }
    }

    async fn handle_export_enter(&mut self, transport: &RuntimeTransport) -> miette::Result<()> {
        let Some(flow) = self.export_flow.as_mut() else {
            return Ok(());
        };
        match flow.step {
            ExportFlowStep::SelectSession => {
                flow.step = ExportFlowStep::Range;
                self.status_message = "enter 1, +N, or -N for the session window".to_owned();
            }
            ExportFlowStep::Range => {
                if let Err(error) = parse_session_range(&flow.range_input) {
                    flow.error = Some(error.to_string());
                    self.status_message = "invalid export range".to_owned();
                } else {
                    flow.step = ExportFlowStep::Destination;
                    self.status_message = "enter an absolute export directory".to_owned();
                }
            }
            ExportFlowStep::Destination => {
                if !Path::new(&flow.destination_input).is_absolute() {
                    flow.error = Some("destination must be an absolute path".to_owned());
                    self.status_message = "absolute path required".to_owned();
                } else if flow.destination_input.trim().is_empty() {
                    flow.error = Some("destination must not be empty".to_owned());
                } else {
                    flow.step = ExportFlowStep::Review;
                    self.status_message = "review export".to_owned();
                }
            }
            ExportFlowStep::Review => {
                let Some(thread_id) = flow.selected_thread_id() else {
                    flow.error = Some("select a session first".to_owned());
                    flow.step = ExportFlowStep::Error;
                    return Ok(());
                };
                let range = match parse_session_range(&flow.range_input) {
                    Ok(range) => range,
                    Err(error) => {
                        flow.error = Some(error.to_string());
                        flow.step = ExportFlowStep::Error;
                        return Ok(());
                    }
                };
                let destination = PathBuf::from(&flow.destination_input);
                flow.step = ExportFlowStep::Running;
                flow.error = None;
                let result = DebugExportCoordinator::new(transport)
                    .export(DebugExportRequest {
                        selection: golutra_protocol::SessionWindowRequest {
                            anchor_thread_id: thread_id,
                            range,
                        },
                        destination,
                    })
                    .await;
                if let Some(flow) = self.export_flow.as_mut() {
                    match result {
                        Ok(receipt) => {
                            self.status_message = "export complete".to_owned();
                            flow.receipt = Some(receipt);
                            flow.step = ExportFlowStep::Completed;
                        }
                        Err(error) => {
                            self.status_message = "export failed".to_owned();
                            flow.error = Some(error.to_string());
                            flow.step = ExportFlowStep::Error;
                        }
                    }
                }
            }
            ExportFlowStep::Completed | ExportFlowStep::Error => {
                self.export_flow = None;
                self.status_message = "export closed".to_owned();
            }
            ExportFlowStep::Running => {
                self.status_message = "export is still running".to_owned();
            }
        }
        Ok(())
    }

    fn start_new_session(&mut self) {
        self.thread_id = ThreadId::new();
        self.session_id = SessionId::new();
        self.task_id = None;
        self.projection = None;
        self.developer_projection = None;
        self.developer_error = None;
        self.developer_scroll.reset(0);
        self.developer_load_requested = false;
        self.events.clear();
        self.command_messages.clear();
        self.input.clear();
        self.export_flow = None;
        self.reset_slash_selection();
        self.reset_history_window();
        self.resume_picker = None;
        self.status_message = "new session".to_owned();
        self.reset_transcript_view();
    }

    async fn resume_thread(
        &mut self,
        transport: &RuntimeTransport,
        thread_id: ThreadId,
    ) -> miette::Result<()> {
        let thread = transport
            .resume_thread(thread_id)
            .await
            .map_err(|error| miette::miette!("{error}"))?;
        self.thread_id = thread.thread_id;
        self.session_id = thread.session_id;
        self.task_id = None;
        self.projection = None;
        self.developer_projection = None;
        self.developer_error = None;
        self.developer_scroll.reset(0);
        self.developer_load_requested = false;
        self.events.clear();
        self.command_messages.clear();
        self.input.clear();
        self.export_flow = None;
        self.reset_slash_selection();
        self.reset_history_window();
        self.resume_picker = None;
        self.status_message = format!("resumed {}", short_id(&thread.thread_id.to_string()));
        self.reset_transcript_view();
        self.refresh(transport).await
    }

    async fn resume_selected_thread(&mut self, transport: &RuntimeTransport) -> miette::Result<()> {
        let Some(thread_id) = self
            .resume_picker
            .as_ref()
            .and_then(ResumePickerState::selected_thread_id)
        else {
            return Ok(());
        };
        self.resume_thread(transport, thread_id).await
    }

    fn move_resume_selection(&mut self, direction: ResumeSelectionDirection) {
        if let Some(picker) = &mut self.resume_picker {
            picker.move_selection(direction);
        }
    }

    fn close_resume_picker(&mut self) {
        self.resume_picker = None;
        self.status_message = "resume cancelled".to_owned();
    }

    fn open_auth_dialog(&mut self) {
        self.resume_picker = None;
        self.input.clear();
        self.auth_dialog = Some(AuthDialogState::new());
        self.status_message = "connect a provider".to_owned();
    }

    async fn execute_auth_command(
        &mut self,
        transport: &RuntimeTransport,
        command: SlashAuthCommand,
    ) -> miette::Result<()> {
        match command {
            SlashAuthCommand::Setup => {
                self.open_auth_dialog();
            }
            SlashAuthCommand::Status => {
                self.refresh_provider_status();
                self.push_system_message("Auth status", vec![self.provider_message.clone()]);
            }
            SlashAuthCommand::Protocols => {
                let lines = provider_protocol_catalog()
                    .into_iter()
                    .map(|protocol| {
                        format!(
                            "{}  {}  {}",
                            protocol.protocol.id(),
                            protocol.status,
                            protocol.notes
                        )
                    })
                    .collect::<Vec<_>>();
                self.push_system_message("Auth protocols", lines);
            }
            SlashAuthCommand::Mock => {
                apply_auth_mock()?;
                notify_runtime_provider_configured(transport, self.session_id).await?;
                self.refresh_provider_status();
                self.auth_dialog = None;
                self.push_system_message(
                    "Auth updated",
                    vec!["global provider switched to mock".to_owned()],
                );
            }
            SlashAuthCommand::Use { profile, scope } => {
                let paths = provider_paths_for_tui()?;
                let cwd = provider_cwd_for_tui(transport)?;
                let selected_profile = profile.clone();
                match update_provider_settings_verified(
                    &paths,
                    cwd,
                    move |user_settings| {
                        if provider_scope(scope) == ProviderConfigScope::Workspace {
                            return Err(golutra_config::ConfigError::Validation(
                                "workspace provider config is no longer supported; use global user provider config"
                                    .to_owned(),
                            ));
                        }
                        user_settings.set_active_profile(selected_profile)?;
                        Ok(())
                    },
                )
                .await
                {
                    Ok(()) => {
                        notify_runtime_provider_configured(transport, self.session_id).await?;
                        self.refresh_provider_status();
                        self.push_system_message(
                            "Auth updated",
                            vec![format!("active provider profile set to {profile}")],
                        );
                    }
                    Err(error) => {
                        self.push_system_message("Auth failed", vec![error.to_string()]);
                        self.status_message = "provider setup failed".to_owned();
                    }
                }
            }
            SlashAuthCommand::Login(login) => match apply_auth_login(transport, *login).await {
                Ok(()) => {
                    notify_runtime_provider_configured(transport, self.session_id).await?;
                    self.refresh_provider_status();
                    self.auth_dialog = None;
                    self.push_system_message(
                        "Auth updated",
                        vec![
                            "provider profile saved".to_owned(),
                            self.provider_message.clone(),
                        ],
                    );
                }
                Err(error) => {
                    self.push_system_message("Auth failed", vec![error.to_string()]);
                    self.status_message = "provider setup failed".to_owned();
                }
            },
            SlashAuthCommand::OAuthLogin(command) => {
                self.start_oauth_login(transport, *command)?;
            }
            SlashAuthCommand::Logout { profile } => {
                self.start_auth_logout(transport, profile)?;
            }
        }
        Ok(())
    }

    fn start_oauth_login(
        &mut self,
        transport: &RuntimeTransport,
        command: OAuthLoginCommand,
    ) -> miette::Result<()> {
        let cwd = provider_cwd_for_tui(transport)?.to_path_buf();
        let descriptor_path = resolve_auth_descriptor_path(&cwd, &command.descriptor_path);
        let descriptor = load_oauth_descriptor_for_tui(&descriptor_path)
            .map_err(|error| miette::miette!("{error}"))?;
        self.start_oauth_login_with_descriptor(transport, descriptor, command)
    }

    fn start_builtin_oauth_login(
        &mut self,
        transport: &RuntimeTransport,
        method: BuiltinOAuthMethod,
    ) -> miette::Result<()> {
        method
            .validate()
            .map_err(|error| miette::miette!("{error}"))?;
        let command = OAuthLoginCommand {
            descriptor_path: format!("builtin:{}:{}", method.provider_id, method.method_id),
            flow: method.flow,
            profile: method.profile,
            protocol: method.protocol,
            base_url: method.base_url,
            model: method.default_model,
            credential_store: default_auth_credential_store(),
            no_open_browser: false,
            generation_config: None,
        };
        self.start_oauth_login_with_descriptor(transport, method.descriptor, command)
    }

    fn start_oauth_login_with_descriptor(
        &mut self,
        transport: &RuntimeTransport,
        descriptor: OAuthProviderDescriptor,
        command: OAuthLoginCommand,
    ) -> miette::Result<()> {
        if self.auth_operation.is_some() {
            self.status_message = "an auth operation is already running".to_owned();
            return Ok(());
        }
        let paths = provider_paths_for_tui()?;
        let cwd = provider_cwd_for_tui(transport)?.to_path_buf();
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let (progress_tx, progress) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            run_oauth_login_task(
                paths,
                cwd,
                descriptor,
                command,
                task_cancellation,
                progress_tx,
            )
            .await
        });
        self.auth_operation = Some(PendingAuthOperation {
            cancellation,
            progress,
            task,
        });
        self.status_message = "starting OAuth authorization".to_owned();
        Ok(())
    }

    fn start_auth_logout(
        &mut self,
        transport: &RuntimeTransport,
        profile: Option<String>,
    ) -> miette::Result<()> {
        if self.auth_operation.is_some() {
            self.status_message = "an auth operation is already running".to_owned();
            return Ok(());
        }
        let paths = provider_paths_for_tui()?;
        let cwd = provider_cwd_for_tui(transport)?.to_path_buf();
        let profile = match profile {
            Some(profile) => profile,
            None => load_provider_settings(&paths)
                .map_err(|error| miette::miette!("{error}"))?
                .active_profile
                .ok_or_else(|| miette::miette!("no active provider profile to log out"))?,
        };
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let (_progress_tx, progress) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            if task_cancellation.is_cancelled() {
                return Err("provider logout cancelled".to_owned());
            }
            let result = logout_provider_profile_verified(&paths, &cwd, profile.clone()).await;
            if let Err(error) = result {
                return Err(error.to_string());
            }
            Ok(AuthTaskOutcome {
                title: "Auth updated".to_owned(),
                body: vec![format!("provider profile {profile} logged out")],
            })
        });
        self.auth_operation = Some(PendingAuthOperation {
            cancellation,
            progress,
            task,
        });
        self.status_message = "logging out provider".to_owned();
        Ok(())
    }

    async fn poll_auth_operation(&mut self, transport: &RuntimeTransport) {
        let mut progress_items = Vec::new();
        let finished = if let Some(operation) = &mut self.auth_operation {
            while let Ok(progress) = operation.progress.try_recv() {
                progress_items.push(progress);
            }
            operation.task.is_finished()
        } else {
            false
        };
        for progress in progress_items {
            self.push_system_message(progress.title, progress.body);
        }
        if !finished {
            return;
        }
        let Some(operation) = self.auth_operation.take() else {
            return;
        };
        match operation.task.await {
            Ok(Ok(outcome)) => {
                if let Err(error) =
                    notify_runtime_provider_configured(transport, self.session_id).await
                {
                    self.push_system_message("Auth failed", vec![error.to_string()]);
                    self.status_message = "provider runtime reload failed".to_owned();
                    return;
                }
                self.refresh_provider_status();
                self.push_system_message(outcome.title, outcome.body);
            }
            Ok(Err(error)) => {
                self.push_system_message("Auth failed", vec![error]);
                self.status_message = "provider auth failed".to_owned();
            }
            Err(error) if error.is_cancelled() => {
                self.push_system_message(
                    "Auth cancelled",
                    vec!["authorization stopped".to_owned()],
                );
            }
            Err(error) => {
                self.push_system_message("Auth failed", vec![format!("auth task failed: {error}")]);
            }
        }
    }

    async fn abort(&mut self, transport: &RuntimeTransport) -> miette::Result<()> {
        let ack = transport
            .send_command(session_command(
                self.session_id,
                SessionCommandKind::Abort,
                json!({}),
            ))
            .await
            .map_err(|error| miette::miette!("{error}"))?;
        self.status_message = ack.reason.unwrap_or_else(|| "abort accepted".to_owned());
        self.refresh(transport).await
    }

    async fn send_control_command(
        &mut self,
        transport: &RuntimeTransport,
        kind: SessionCommandKind,
    ) -> miette::Result<()> {
        let ack = transport
            .send_command(session_command(self.session_id, kind, json!({})))
            .await
            .map_err(|error| miette::miette!("{error}"))?;
        self.status_message = compact_ack_reason(&ack.reason);
        self.refresh(transport).await
    }

    async fn resolve_pending_approval(
        &mut self,
        transport: &RuntimeTransport,
        kind: SessionCommandKind,
    ) -> miette::Result<()> {
        let approval_id = self
            .projection
            .as_ref()
            .and_then(|projection| projection.pending_approval.clone());
        let payload = approval_id.map_or_else(
            || json!({}),
            |approval_id| json!({"approval_id": approval_id}),
        );
        let ack = transport
            .send_command(session_command(self.session_id, kind, payload))
            .await
            .map_err(|error| miette::miette!("{error}"))?;
        self.status_message = compact_ack_reason(&ack.reason);
        self.refresh(transport).await
    }

    async fn interrupt_or_quit(&mut self, transport: &RuntimeTransport) -> miette::Result<()> {
        if self.quit_shortcut_is_active() {
            self.should_quit = true;
            return Ok(());
        }
        self.arm_quit_shortcut();
        if let Some(operation) = &self.auth_operation {
            operation.cancellation.cancel();
            self.status_message =
                "auth cancellation requested; press Ctrl+C again to quit".to_owned();
        } else if has_active_task(self) {
            self.abort(transport).await?;
            self.status_message = "interrupt requested; press Ctrl+C again to quit".to_owned();
        } else {
            self.status_message = "press Ctrl+C again to quit".to_owned();
        }
        Ok(())
    }

    fn arm_quit_shortcut(&mut self) {
        self.quit_shortcut_expires_at = Instant::now().checked_add(Duration::from_secs(2));
    }

    fn quit_shortcut_is_active(&self) -> bool {
        self.quit_shortcut_expires_at
            .is_some_and(|expires_at| Instant::now() < expires_at)
    }

    fn push_system_message(&mut self, title: impl Into<String>, body: Vec<String>) {
        self.status_message = title.into();
        self.command_messages.push(TranscriptItem {
            role: TranscriptRole::System,
            title: self.status_message.clone(),
            body,
        });
        if self.command_messages.len() > 12 {
            self.command_messages
                .drain(0..self.command_messages.len().saturating_sub(12));
        }
        self.sync_transcript_row_count(self.transcript_scroll.row_count);
        self.clamp_transcript_scroll();
    }
}

#[tokio::main]
async fn main() -> miette::Result<()> {
    let args = Args::parse();
    let task_id = parse_task_id(args.task_id.as_deref())?;
    let cwd = args
        .cwd
        .clone()
        .map_or_else(std::env::current_dir, Ok)
        .map_err(|error| miette::miette!("{error}"))?;
    let transport = if let Some(base_url) = args.connect.clone() {
        RuntimeTransport::connect(base_url, &cwd).await
    } else if args.daemon {
        RuntimeTransport::local_daemon(&cwd).await
    } else {
        RuntimeTransport::for_cwd(&cwd).await
    }
    .map_err(|error| miette::miette!("{error}"))?;
    let (thread_id, session_id) = initial_session(args.session_id.as_deref(), &transport).await?;
    let provider_status = current_provider_ui_status();
    let runtime_cwd = transport.cwd().unwrap_or(&cwd).to_path_buf();
    let auth_dialog = initial_auth_dialog();
    let app = TuiApp::new(
        thread_id,
        session_id,
        task_id,
        args.debug,
        provider_status.message,
        auth_dialog,
    )
    .with_footer_context(runtime_cwd, provider_status.model);
    let mut terminal = setup_terminal()?;
    let result = run_app(&mut terminal, app, transport).await;
    restore_terminal(&mut terminal)?;
    result
}

async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut app: TuiApp,
    transport: RuntimeTransport,
) -> miette::Result<()> {
    let mut subscribed_session = app.session_id;
    let mut subscribed_task = app.task_id;
    app.load_recent_history(&transport).await?;
    let mut subscription = transport
        .subscribe(EventFilter {
            session_id: subscribed_session,
            task_id: subscribed_task,
            after_sequence_no: app.cursor,
        })
        .await
        .map_err(|error| miette::miette!("{error}"))?;
    app.refresh(&transport).await?;
    let tick_rate = Duration::from_millis(250);

    while !app.should_quit {
        terminal
            .draw(|frame| draw_ui(frame, &mut app))
            .map_err(|error| miette::miette!("{error}"))?;

        if event::poll(tick_rate).map_err(|error| miette::miette!("{error}"))? {
            let event = event::read().map_err(|error| miette::miette!("{error}"))?;
            match event {
                CrosstermEvent::Key(key) => {
                    handle_key(key, &mut app, &transport).await?;
                }
                CrosstermEvent::Mouse(mouse) => {
                    handle_mouse(mouse, &mut app);
                }
                CrosstermEvent::Paste(pasted) => {
                    handle_paste(&pasted, &mut app);
                }
                _ => {}
            }
        }

        if app.history_load_requested {
            app.load_older_history(&transport).await?;
        }
        if app.developer_load_requested {
            app.load_older_debug_history(&transport).await?;
        }

        if app.session_id != subscribed_session || app.task_id != subscribed_task {
            subscribed_session = app.session_id;
            subscribed_task = app.task_id;
            app.load_recent_history(&transport).await?;
            subscription = transport
                .subscribe(EventFilter {
                    session_id: subscribed_session,
                    task_id: subscribed_task,
                    after_sequence_no: app.cursor,
                })
                .await
                .map_err(|error| miette::miette!("{error}"))?;
            app.refresh(&transport).await?;
        }

        let mut received_event = false;
        loop {
            match subscription.try_recv() {
                Ok(Ok(event)) => {
                    app.apply_runtime_event(event);
                    received_event = true;
                }
                Ok(Err(error)) => {
                    app.status_message = format!("event stream reconnecting: {error}");
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    app.status_message = "event stream disconnected".to_owned();
                    break;
                }
            }
        }
        if received_event {
            app.refresh(&transport).await?;
        }
        app.poll_auth_operation(&transport).await;
    }

    Ok(())
}

async fn handle_key(
    key: KeyEvent,
    app: &mut TuiApp,
    transport: &RuntimeTransport,
) -> miette::Result<()> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return Ok(());
    }
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return app.interrupt_or_quit(transport).await;
    }
    if app.export_flow.is_some() {
        return handle_export_key(key, app, transport).await;
    }
    if app.resume_picker.is_some() {
        return handle_resume_picker_key(key, app, transport).await;
    }
    if app.auth_dialog.is_some() {
        return handle_auth_dialog_key(key, app, transport).await;
    }
    if app.input.is_empty()
        && app
            .projection
            .as_ref()
            .and_then(|projection| projection.pending_approval.as_ref())
            .is_some()
    {
        match key.code {
            KeyCode::Char('y') if key.modifiers.is_empty() => {
                return app
                    .resolve_pending_approval(transport, SessionCommandKind::Approve)
                    .await;
            }
            KeyCode::Char('n') if key.modifiers.is_empty() => {
                return app
                    .resolve_pending_approval(transport, SessionCommandKind::Deny)
                    .await;
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Esc => {
            if !app.input.is_empty() {
                app.input.clear();
                app.reset_slash_selection();
                app.status_message = "input cleared".to_owned();
            } else {
                app.status_message = "press Ctrl+C twice to quit".to_owned();
            }
        }
        KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.move_to_start();
        }
        KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.move_to_end();
        }
        KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.move_left();
        }
        KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.move_right();
        }
        KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.delete_backward();
            app.reset_slash_selection();
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.clear();
            app.reset_slash_selection();
        }
        KeyCode::Tab => {
            if app.move_slash_selection(ResumeSelectionDirection::Next) {
                app.status_message = "slash command selected".to_owned();
            }
        }
        KeyCode::Up => {
            app.move_slash_selection(ResumeSelectionDirection::Previous);
        }
        KeyCode::Down => {
            app.move_slash_selection(ResumeSelectionDirection::Next);
        }
        KeyCode::PageUp => {
            app.scroll_transcript(TranscriptScrollAction::PageUp, transcript_page_rows(app));
        }
        KeyCode::PageDown => {
            app.scroll_transcript(TranscriptScrollAction::PageDown, transcript_page_rows(app));
        }
        KeyCode::Home => {
            if app.input.is_empty() {
                app.scroll_transcript(TranscriptScrollAction::Top, transcript_page_rows(app));
            } else {
                app.input.move_to_start();
            }
        }
        KeyCode::End => {
            if app.input.is_empty() {
                app.scroll_transcript(TranscriptScrollAction::Bottom, transcript_page_rows(app));
            } else {
                app.input.move_to_end();
            }
        }
        KeyCode::Left => {
            app.input.move_left();
        }
        KeyCode::Right => {
            app.input.move_right();
        }
        KeyCode::Enter => {
            if !app.accept_slash_candidate(transport).await? {
                app.send_prompt(transport).await?;
            }
        }
        KeyCode::Backspace => {
            app.input.delete_backward();
            app.reset_slash_selection();
        }
        KeyCode::Delete => {
            app.input.delete_forward();
            app.reset_slash_selection();
        }
        KeyCode::Char(character)
            if !key.modifiers.intersects(
                KeyModifiers::CONTROL
                    | KeyModifiers::ALT
                    | KeyModifiers::SUPER
                    | KeyModifiers::HYPER
                    | KeyModifiers::META,
            ) =>
        {
            app.input.insert_char(character);
            app.reset_slash_selection();
        }
        _ => {}
    }
    Ok(())
}

async fn handle_export_key(
    key: KeyEvent,
    app: &mut TuiApp,
    transport: &RuntimeTransport,
) -> miette::Result<()> {
    let step = app.export_flow.as_ref().map(|flow| flow.step);
    match step {
        Some(ExportFlowStep::SelectSession) => match key.code {
            KeyCode::Esc => app.close_export_flow(),
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(flow) = &mut app.export_flow {
                    flow.picker
                        .move_selection(ResumeSelectionDirection::Previous);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(flow) = &mut app.export_flow {
                    flow.picker.move_selection(ResumeSelectionDirection::Next);
                }
            }
            KeyCode::Enter => app.handle_export_enter(transport).await?,
            KeyCode::Char(character) if character.is_ascii_digit() => {
                if let Some(index) = character
                    .to_digit(10)
                    .and_then(|digit| digit.checked_sub(1))
                    && let Some(flow) = &mut app.export_flow
                    && (index as usize) < flow.picker.items.len()
                {
                    flow.picker.selected = index as usize;
                    app.handle_export_enter(transport).await?;
                }
            }
            _ => {}
        },
        Some(ExportFlowStep::Range | ExportFlowStep::Destination) => match key.code {
            KeyCode::Esc => {
                if let Some(flow) = &mut app.export_flow {
                    flow.step = if flow.step == ExportFlowStep::Range {
                        ExportFlowStep::SelectSession
                    } else {
                        ExportFlowStep::Range
                    };
                    flow.error = None;
                }
            }
            KeyCode::Enter => app.handle_export_enter(transport).await?,
            KeyCode::Backspace => {
                if let Some(input) = app.export_input_mut() {
                    input.pop();
                }
            }
            KeyCode::Char(character)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL
                        | KeyModifiers::ALT
                        | KeyModifiers::SUPER
                        | KeyModifiers::HYPER
                        | KeyModifiers::META,
                ) =>
            {
                if let Some(input) = app.export_input_mut() {
                    input.push(character);
                }
            }
            _ => {}
        },
        Some(ExportFlowStep::Review)
        | Some(ExportFlowStep::Completed)
        | Some(ExportFlowStep::Error) => match key.code {
            KeyCode::Esc | KeyCode::Enter => app.handle_export_enter(transport).await?,
            _ => {}
        },
        Some(ExportFlowStep::Running) => {
            if key.code == KeyCode::Esc {
                app.status_message = "export cannot be cancelled after writing started".to_owned();
            }
        }
        None => {}
    }
    Ok(())
}

fn handle_paste(pasted: &str, app: &mut TuiApp) {
    if app.resume_picker.is_some() {
        return;
    }

    if app.export_flow.is_some() {
        if let Some(input) = app.export_input_mut() {
            input.push_str(&pasted.replace("\r\n", "\n").replace('\r', "\n"));
        }
        return;
    }

    let normalized = pasted.replace("\r\n", "\n").replace('\r', "\n");
    if let Some(dialog) = &mut app.auth_dialog {
        if let Some(input) = dialog.current_input_mut() {
            input.push_str(&normalized.replace('\n', ""));
            dialog.error = None;
        }
        return;
    }

    app.input.insert_str(&normalized);
    app.reset_slash_selection();
}

fn handle_mouse(mouse: MouseEvent, app: &mut TuiApp) {
    let target = app.layout.hit_test(mouse.column, mouse.row, app);
    match mouse.kind {
        MouseEventKind::ScrollUp => match target {
            UiHitTarget::Developer => {
                let rows = app
                    .layout
                    .developer
                    .map(|area| area.height.saturating_sub(1) as usize)
                    .unwrap_or(1);
                app.scroll_developer(TranscriptScrollAction::LineUp, rows);
            }
            UiHitTarget::Transcript => {
                let rows = app.layout.transcript.height.saturating_sub(1) as usize;
                app.scroll_transcript(TranscriptScrollAction::LineUp, rows);
            }
            _ => {}
        },
        MouseEventKind::ScrollDown => match target {
            UiHitTarget::Developer => {
                let rows = app
                    .layout
                    .developer
                    .map(|area| area.height.saturating_sub(1) as usize)
                    .unwrap_or(1);
                app.scroll_developer(TranscriptScrollAction::LineDown, rows);
            }
            UiHitTarget::Transcript => {
                let rows = app.layout.transcript.height.saturating_sub(1) as usize;
                app.scroll_transcript(TranscriptScrollAction::LineDown, rows);
            }
            _ => {}
        },
        _ => {}
    }
}

async fn handle_resume_picker_key(
    key: KeyEvent,
    app: &mut TuiApp,
    transport: &RuntimeTransport,
) -> miette::Result<()> {
    match key.code {
        KeyCode::Esc => app.close_resume_picker(),
        KeyCode::Up | KeyCode::Char('k') => {
            app.move_resume_selection(ResumeSelectionDirection::Previous);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.move_resume_selection(ResumeSelectionDirection::Next);
        }
        KeyCode::Enter => {
            app.resume_selected_thread(transport).await?;
        }
        KeyCode::Char(character) if character.is_ascii_digit() => {
            if let Some(index) = character
                .to_digit(10)
                .and_then(|digit| digit.checked_sub(1))
                && let Some(picker) = &mut app.resume_picker
                && (index as usize) < picker.items.len()
            {
                picker.selected = index as usize;
                app.resume_selected_thread(transport).await?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn setup_terminal() -> miette::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().map_err(|error| miette::miette!("{error}"))?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, event::EnableMouseCapture)
        .map_err(|error| miette::miette!("{error}"))?;
    let _ = execute!(stdout, EnableBracketedPaste, SetCursorStyle::SteadyBar);
    Terminal::new(CrosstermBackend::new(stdout)).map_err(|error| miette::miette!("{error}"))
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> miette::Result<()> {
    disable_raw_mode().map_err(|error| miette::miette!("{error}"))?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        event::DisableMouseCapture,
        SetCursorStyle::DefaultUserShape,
        LeaveAlternateScreen
    )
    .map_err(|error| miette::miette!("{error}"))?;
    terminal
        .show_cursor()
        .map_err(|error| miette::miette!("{error}"))
}

fn slash_help_lines() -> Vec<String> {
    vec![
        "/new  start a new session".to_owned(),
        "/resume  open current cwd session list".to_owned(),
        "/resume <thread-id>  resume a specific current-cwd thread".to_owned(),
        "/export  export selected sessions and governed runtime facts".to_owned(),
        "/threads [limit]  list recent threads for this cwd".to_owned(),
        "/fork <thread-id> [--from-turn <turn-id>]  fork history and switch to it".to_owned(),
        "/auth status  show provider onboarding state".to_owned(),
        "/auth setup  open provider setup".to_owned(),
        "/auth protocols  list registered provider protocols".to_owned(),
        "/auth mock  switch global provider to mock".to_owned(),
        "/auth login --base-url <url> --model <model> [--api-key <key>|--api-key-env <env>] [--scope user]".to_owned(),
        "/auth use <profile> [user]  activate saved global provider profile".to_owned(),
        "/status  show current session/task status".to_owned(),
        "/debug  toggle developer runtime facts and event view".to_owned(),
        "/abort  abort active task".to_owned(),
        "/pause  pause active task".to_owned(),
        "/continue  resume paused task".to_owned(),
        "/approve  approve pending tool execution".to_owned(),
        "/deny  deny pending tool execution".to_owned(),
        "/compact  compact durable conversation history".to_owned(),
        "/clear  clear local command messages".to_owned(),
        "/quit  leave TUI".to_owned(),
    ]
}

#[cfg(test)]
mod tests;
