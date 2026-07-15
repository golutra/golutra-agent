use std::{
    io::{self, Stdout},
    path::{Path, PathBuf},
    sync::LazyLock,
    time::{Duration, Instant},
};

use clap::Parser;
use crossterm::{
    event::{
        self, Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use golutra_auth::{
    CredentialRef, CredentialSource, OAuthFlow, OAuthProviderDescriptor, SecretKind,
};
use golutra_client::{RuntimeClient, RuntimeTransport};
use golutra_config::{
    BuiltinOAuthMethod, ProviderConfigPaths, ProviderConfigScope, ProviderInstallPlan,
    ProviderProfile, apply_oauth_provider_install_plan_verified,
    apply_provider_install_plan_verified, generate_custom_provider_api_key_env,
    load_provider_settings, logout_provider_profile_verified, provider_auth_service,
    provider_onboarding_state, provider_protocol_has_runtime_adapter,
    update_provider_settings_verified,
};
use golutra_core::{ActorKind, QueryId, SessionId, TaskId, ThreadId};
use golutra_llm::{ProviderGenerationConfig, ProviderProtocol, provider_protocol_catalog};
use golutra_protocol::{
    EventFilter, RuntimeEvent, RuntimeEventType, RuntimeQuery, RuntimeQueryKind,
    SessionCommandKind, UserProjection,
};
use golutra_tui::{
    AuthConfigScope, AuthCredentialStore, OAuthLoginCommand, OpenAiCompatibleLogin,
    SlashAuthCommand, SlashCommand, SlashCommandCandidate, SlashInput, parse_slash_input,
    slash_command_candidates,
};
use ratatui::{Terminal, backend::CrosstermBackend};
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

static TUI_ACTOR_ID: LazyLock<String> =
    LazyLock::new(|| format!("golutra-tui-{}-{}", std::process::id(), Uuid::now_v7()));

mod auth_flow;
mod auth_state;
mod developer;
mod render;
mod session;
pub(crate) use auth_flow::*;
pub(crate) use auth_state::*;
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
    auth_dialog: Option<AuthDialogState>,
    auth_operation: Option<PendingAuthOperation>,
    input: String,
    slash_selected: usize,
    status_message: String,
    provider_message: String,
    provider_model: String,
    workspace_path: PathBuf,
    debug_mode: bool,
    cursor: Option<u64>,
    transcript_scroll_offset: usize,
    transcript_row_count: usize,
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
            auth_dialog,
            auth_operation: None,
            input: String::new(),
            slash_selected: 0,
            status_message: String::new(),
            provider_message,
            provider_model,
            workspace_path,
            debug_mode,
            cursor: None,
            transcript_scroll_offset: 0,
            transcript_row_count: 0,
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
        let previous_row_count = self.transcript_row_count;
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
        if self.debug_mode {
            match load_debug_projection(transport, self.session_id, self.task_id).await {
                Ok(projection) => {
                    self.developer_projection = Some(projection);
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
        }
        self.sync_transcript_row_count(previous_row_count);
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
        if self.resume_picker.is_some() {
            self.status_message = "select a session with arrow keys or Esc".to_owned();
            self.input.clear();
            return Ok(());
        }

        let input = self.input.trim().to_owned();
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
        let trimmed_input = self.input.trim();
        if trimmed_input.starts_with(&format!("{} ", candidate.command)) {
            return Ok(false);
        }
        if candidate.execute_on_select {
            self.input = candidate.command;
            self.send_prompt(transport).await?;
        } else {
            self.input = format!("{} ", candidate.command);
            self.slash_selected = 0;
            self.status_message = "complete slash command arguments".to_owned();
        }
        Ok(true)
    }

    fn slash_candidates(&self) -> Vec<SlashCommandCandidate> {
        if self.auth_operation.is_some()
            || self.auth_dialog.is_some()
            || self.resume_picker.is_some()
        {
            return Vec::new();
        }
        slash_command_candidates(&self.input)
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
        self.transcript_scroll_offset = 0;
        self.transcript_row_count = transcript_rows(self).len();
    }

    fn sync_transcript_row_count(&mut self, previous_row_count: usize) {
        let current_row_count = transcript_rows(self).len();
        if self.transcript_scroll_offset > 0 && current_row_count > previous_row_count {
            self.transcript_scroll_offset = self
                .transcript_scroll_offset
                .saturating_add(current_row_count - previous_row_count);
        }
        self.transcript_row_count = current_row_count;
        self.clamp_transcript_scroll();
    }

    fn scroll_transcript(&mut self, action: TranscriptScrollAction, visible_rows: usize) {
        if self.auth_dialog.is_some() || self.resume_picker.is_some() || self.debug_mode {
            return;
        }
        let page = visible_rows.max(1);
        match action {
            TranscriptScrollAction::LineUp => {
                self.transcript_scroll_offset = self.transcript_scroll_offset.saturating_add(1);
            }
            TranscriptScrollAction::LineDown => {
                self.transcript_scroll_offset = self.transcript_scroll_offset.saturating_sub(1);
            }
            TranscriptScrollAction::PageUp => {
                self.transcript_scroll_offset = self.transcript_scroll_offset.saturating_add(page);
            }
            TranscriptScrollAction::PageDown => {
                self.transcript_scroll_offset = self.transcript_scroll_offset.saturating_sub(page);
            }
            TranscriptScrollAction::Top => {
                self.transcript_scroll_offset = self.max_transcript_scroll_offset(visible_rows);
            }
            TranscriptScrollAction::Bottom => {
                self.transcript_scroll_offset = 0;
            }
        }
        self.clamp_transcript_scroll_for_rows(visible_rows);
        self.status_message = transcript_scroll_status(self.transcript_scroll_offset);
    }

    fn clamp_transcript_scroll(&mut self) {
        self.transcript_scroll_offset = self
            .transcript_scroll_offset
            .min(self.transcript_row_count.saturating_sub(1));
    }

    fn clamp_transcript_scroll_for_rows(&mut self, visible_rows: usize) {
        self.transcript_scroll_offset = self
            .transcript_scroll_offset
            .min(self.max_transcript_scroll_offset(visible_rows));
    }

    fn max_transcript_scroll_offset(&self, visible_rows: usize) -> usize {
        self.transcript_row_count
            .saturating_sub(visible_rows.max(1))
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
                self.events.clear();
                self.command_messages.clear();
                self.input.clear();
                self.reset_slash_selection();
                self.cursor = None;
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

    fn start_new_session(&mut self) {
        self.thread_id = ThreadId::new();
        self.session_id = SessionId::new();
        self.task_id = None;
        self.projection = None;
        self.developer_projection = None;
        self.developer_error = None;
        self.events.clear();
        self.command_messages.clear();
        self.input.clear();
        self.reset_slash_selection();
        self.cursor = None;
        self.resume_picker = None;
        self.debug_mode = false;
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
        self.events.clear();
        self.command_messages.clear();
        self.input.clear();
        self.reset_slash_selection();
        self.cursor = None;
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

    async fn poll_auth_operation(&mut self) {
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
        self.transcript_row_count = transcript_rows(self).len();
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
            .draw(|frame| draw_ui(frame, &app))
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
                _ => {}
            }
        }

        if app.session_id != subscribed_session || app.task_id != subscribed_task {
            subscribed_session = app.session_id;
            subscribed_task = app.task_id;
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
        app.poll_auth_operation().await;
    }

    Ok(())
}

async fn handle_key(
    key: KeyEvent,
    app: &mut TuiApp,
    transport: &RuntimeTransport,
) -> miette::Result<()> {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return app.interrupt_or_quit(transport).await;
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
            app.abort(transport).await?;
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
            app.scroll_transcript(TranscriptScrollAction::Top, transcript_page_rows(app));
        }
        KeyCode::End => {
            app.scroll_transcript(TranscriptScrollAction::Bottom, transcript_page_rows(app));
        }
        KeyCode::Enter => {
            if !app.accept_slash_candidate(transport).await? {
                app.send_prompt(transport).await?;
            }
        }
        KeyCode::Backspace => {
            app.input.pop();
            app.reset_slash_selection();
        }
        KeyCode::Char(character) => {
            app.input.push(character);
            app.reset_slash_selection();
        }
        _ => {}
    }
    Ok(())
}

fn handle_mouse(mouse: MouseEvent, app: &mut TuiApp) {
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            app.scroll_transcript(TranscriptScrollAction::LineUp, transcript_page_rows(app));
        }
        MouseEventKind::ScrollDown => {
            app.scroll_transcript(TranscriptScrollAction::LineDown, transcript_page_rows(app));
        }
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
    execute!(stdout, EnterAlternateScreen).map_err(|error| miette::miette!("{error}"))?;
    Terminal::new(CrosstermBackend::new(stdout)).map_err(|error| miette::miette!("{error}"))
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> miette::Result<()> {
    disable_raw_mode().map_err(|error| miette::miette!("{error}"))?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)
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
