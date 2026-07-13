use std::{
    collections::HashSet,
    io::{self, Stdout},
    path::PathBuf,
    sync::LazyLock,
    time::{Duration, Instant},
};

use clap::Parser;
use crossterm::{
    event::{
        self, Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode, size,
    },
};
use golutra_auth::{
    CredentialRef, CredentialSource, OAuthFlow, OAuthProviderDescriptor, SecretKind,
};
use golutra_client::{RuntimeClient, RuntimeTransport};
use golutra_config::{
    BuiltinOAuthMethod, ProviderConfigPaths, ProviderConfigScope, ProviderInstallPlan,
    ProviderProfile, apply_oauth_provider_install_plan_verified,
    apply_provider_install_plan_verified, builtin_oauth_methods_for_provider,
    generate_custom_provider_api_key_env, load_provider_settings, logout_provider_profile_verified,
    provider_auth_service, provider_onboarding_state, provider_protocol_has_runtime_adapter,
    update_provider_settings_verified,
};
use golutra_core::{Actor, ActorKind, CommandId, QueryId, SessionId, TaskId, ThreadId, TurnId};
use golutra_llm::{
    ProviderGenerationConfig, ProviderProtocol, ProviderReasoningEffort, provider_protocol_catalog,
};
use golutra_protocol::{
    EventFilter, RuntimeEvent, RuntimeEventType, RuntimeQuery, RuntimeQueryKind, SessionCommand,
    SessionCommandKind, UserProjection, VisibleStep,
};
use golutra_tui::{
    AuthConfigScope, AuthCredentialStore, OAuthLoginCommand, OpenAiCompatibleLogin,
    SlashAuthCommand, SlashCommand, SlashCommandCandidate, SlashInput, event_timeline_lines,
    parse_slash_input, slash_command_candidates,
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

static TUI_ACTOR_ID: LazyLock<String> =
    LazyLock::new(|| format!("golutra-tui-{}-{}", std::process::id(), Uuid::now_v7()));

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
    events: Vec<Value>,
    command_messages: Vec<TranscriptItem>,
    resume_picker: Option<ResumePickerState>,
    auth_dialog: Option<AuthDialogState>,
    auth_operation: Option<PendingAuthOperation>,
    input: String,
    slash_selected: usize,
    status_message: String,
    provider_message: String,
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

impl TuiApp {
    fn new(
        thread_id: ThreadId,
        session_id: SessionId,
        task_id: Option<TaskId>,
        debug_mode: bool,
        provider_message: String,
        auth_dialog: Option<AuthDialogState>,
    ) -> Self {
        Self {
            thread_id,
            session_id,
            task_id,
            projection: None,
            events: Vec::new(),
            command_messages: Vec::new(),
            resume_picker: None,
            auth_dialog,
            auth_operation: None,
            input: String::new(),
            slash_selected: 0,
            status_message: String::new(),
            provider_message,
            debug_mode,
            cursor: None,
            transcript_scroll_offset: 0,
            transcript_row_count: 0,
            quit_shortcut_expires_at: None,
            should_quit: false,
        }
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
        self.sync_transcript_row_count(previous_row_count);
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
                self.debug_mode = !self.debug_mode;
                self.status_message = if self.debug_mode {
                    "debug timeline visible".to_owned()
                } else {
                    "debug timeline hidden".to_owned()
                };
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
                self.provider_message = provider_status_message();
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
                self.provider_message = provider_status_message();
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
                        self.provider_message = provider_status_message();
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
                    self.provider_message = provider_status_message();
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
                self.provider_message = provider_status_message();
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

#[derive(Debug, Clone)]
struct ResumePickerState {
    items: Vec<ResumeThreadItem>,
    selected: usize,
}

#[derive(Debug, Clone)]
struct ResumeThreadItem {
    thread_id: ThreadId,
    session_id: SessionId,
    title: String,
    preview: String,
}

#[derive(Debug)]
struct PendingAuthOperation {
    cancellation: CancellationToken,
    progress: mpsc::UnboundedReceiver<AuthTaskProgress>,
    task: JoinHandle<Result<AuthTaskOutcome, String>>,
}

#[derive(Debug)]
struct AuthTaskProgress {
    title: String,
    body: Vec<String>,
}

#[derive(Debug)]
struct AuthTaskOutcome {
    title: String,
    body: Vec<String>,
}

#[derive(Debug, Clone)]
struct AuthDialogState {
    step: AuthDialogStep,
    selected: usize,
    provider: Option<AuthProviderPreset>,
    protocol: ProviderProtocol,
    base_url: String,
    model: String,
    api_key: String,
    api_key_env: String,
    credential_store: AuthCredentialStore,
    enable_thinking: bool,
    reasoning_effort: Option<ProviderReasoningEffort>,
    context_window_size: String,
    max_tokens: String,
    advanced_selected: usize,
    review: Option<AuthReview>,
    error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthDialogStep {
    GroupChoice,
    ThirdPartyChoice,
    AuthMethod,
    Protocol,
    BaseUrl,
    CredentialStore,
    ApiKey,
    EnvKey,
    Model,
    AdvancedConfig,
    Review,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthGroupAction {
    Official,
    ThirdParty,
    Custom,
    Mock,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthProviderSource {
    Official,
    ThirdParty,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AuthProviderPreset {
    profile: &'static str,
    title: &'static str,
    detail: &'static str,
    source: AuthProviderSource,
    protocol_options: &'static [ProviderProtocol],
    base_url: Option<&'static str>,
    model: Option<&'static str>,
    recommended_models: &'static [&'static str],
    oauth_provider_id: Option<&'static str>,
    api_key_supported: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AuthReview {
    provider_title: &'static str,
    profile: String,
    protocol: String,
    base_url: String,
    model: String,
    credential: String,
    credential_ref: CredentialRef,
    advanced: String,
    scope: ProviderConfigScope,
    config_path: PathBuf,
    updates_existing_profile: bool,
    preview_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AuthAdvanceAction {
    None,
    SaveMock,
    SaveOpenAiCompatible(Box<OpenAiCompatibleLogin>),
    StartBuiltinOAuth(Box<BuiltinOAuthMethod>),
    Quit,
}

fn default_auth_credential_store() -> AuthCredentialStore {
    #[cfg(test)]
    {
        AuthCredentialStore::Ephemeral
    }
    #[cfg(not(test))]
    {
        AuthCredentialStore::Disk
    }
}

impl AuthDialogState {
    fn new() -> Self {
        Self {
            step: AuthDialogStep::GroupChoice,
            selected: 0,
            provider: None,
            protocol: ProviderProtocol::OpenAiCompatible,
            base_url: String::new(),
            model: String::new(),
            api_key: String::new(),
            api_key_env: String::new(),
            credential_store: default_auth_credential_store(),
            enable_thinking: false,
            reasoning_effort: None,
            context_window_size: String::new(),
            max_tokens: String::new(),
            advanced_selected: 0,
            review: None,
            error: None,
        }
    }

    fn selected_group_action(&self) -> AuthGroupAction {
        match self.selected {
            1 => AuthGroupAction::ThirdParty,
            2 => AuthGroupAction::Custom,
            3 => AuthGroupAction::Mock,
            4 => AuthGroupAction::Quit,
            _ => AuthGroupAction::Official,
        }
    }

    fn selected_third_party_provider(&self) -> AuthProviderPreset {
        THIRD_PARTY_PROVIDER_PRESETS[self
            .selected
            .min(THIRD_PARTY_PROVIDER_PRESETS.len().saturating_sub(1))]
    }

    fn select_provider(&mut self, provider: AuthProviderPreset) {
        self.provider = Some(provider);
        self.protocol = provider
            .protocol_options
            .first()
            .copied()
            .unwrap_or(ProviderProtocol::OpenAiCompatible);
        self.base_url = provider.base_url.unwrap_or_default().to_owned();
        self.model = provider.model.unwrap_or_default().to_owned();
        self.api_key.clear();
        self.api_key_env.clear();
        self.credential_store = default_auth_credential_store();
        self.enable_thinking = false;
        self.reasoning_effort = None;
        self.context_window_size.clear();
        self.max_tokens.clear();
        self.advanced_selected = 0;
        self.review = None;
        self.error = None;
        self.step = if !self.oauth_methods().is_empty() {
            AuthDialogStep::AuthMethod
        } else if provider.protocol_options.len() > 1 {
            AuthDialogStep::Protocol
        } else {
            AuthDialogStep::BaseUrl
        };
        self.selected = 0;
    }

    fn protocol_options(&self) -> &'static [ProviderProtocol] {
        self.provider
            .map(|provider| provider.protocol_options)
            .unwrap_or(&[])
    }

    fn oauth_methods(&self) -> Vec<BuiltinOAuthMethod> {
        self.provider
            .and_then(|provider| provider.oauth_provider_id)
            .map(builtin_oauth_methods_for_provider)
            .unwrap_or_default()
    }

    fn auth_method_count(&self) -> usize {
        self.oauth_methods().len()
            + usize::from(
                self.provider
                    .is_some_and(|provider| provider.api_key_supported),
            )
    }

    fn selected_oauth_method(&self) -> Option<BuiltinOAuthMethod> {
        self.oauth_methods().get(self.selected).cloned()
    }

    fn api_key_method_selected(&self) -> bool {
        let methods = self.oauth_methods();
        self.provider
            .is_some_and(|provider| provider.api_key_supported)
            && self.selected >= methods.len()
    }

    fn selected_protocol(&self) -> ProviderProtocol {
        self.protocol_options()
            .get(self.selected)
            .copied()
            .unwrap_or(self.protocol)
    }

    fn default_base_url_for_protocol(protocol: ProviderProtocol) -> &'static str {
        match protocol {
            ProviderProtocol::OpenAiCompatible => "https://api.openai.com/v1",
            ProviderProtocol::Anthropic => "https://api.anthropic.com/v1",
            ProviderProtocol::Gemini => "https://generativelanguage.googleapis.com/v1beta",
            _ => "",
        }
    }

    fn model_options(&self) -> &'static [&'static str] {
        self.provider
            .map(|provider| provider.recommended_models)
            .unwrap_or(&[])
    }

    fn custom_model_index(&self) -> usize {
        self.model_options().len()
    }

    fn selected_recommended_model(&self) -> Option<&'static str> {
        self.model_options().get(self.selected).copied()
    }

    fn is_custom_model_selected(&self) -> bool {
        self.selected >= self.custom_model_index()
    }

    fn move_selection(&mut self, direction: ResumeSelectionDirection) {
        let last_index = match self.step {
            AuthDialogStep::GroupChoice => AUTH_GROUP_ITEMS.len().saturating_sub(1),
            AuthDialogStep::ThirdPartyChoice => {
                THIRD_PARTY_PROVIDER_PRESETS.len().saturating_sub(1)
            }
            AuthDialogStep::AuthMethod => self.auth_method_count().saturating_sub(1),
            AuthDialogStep::Protocol => self.protocol_options().len().saturating_sub(1),
            AuthDialogStep::CredentialStore => 1,
            AuthDialogStep::Model => self.custom_model_index(),
            AuthDialogStep::AdvancedConfig => AUTH_ADVANCED_ITEMS.saturating_sub(1),
            AuthDialogStep::BaseUrl
            | AuthDialogStep::ApiKey
            | AuthDialogStep::EnvKey
            | AuthDialogStep::Review => 0,
        };
        let current = if self.step == AuthDialogStep::AdvancedConfig {
            self.advanced_selected
        } else {
            self.selected
        };
        let target = match direction {
            ResumeSelectionDirection::Previous => current.saturating_sub(1),
            ResumeSelectionDirection::Next => (current + 1).min(last_index),
        };
        if self.step == AuthDialogStep::AdvancedConfig {
            self.advanced_selected = target;
        } else {
            self.selected = target;
        }
        self.error = None;
    }

    fn current_input_mut(&mut self) -> Option<&mut String> {
        match self.step {
            AuthDialogStep::BaseUrl => Some(&mut self.base_url),
            AuthDialogStep::ApiKey => Some(&mut self.api_key),
            AuthDialogStep::EnvKey => Some(&mut self.api_key_env),
            AuthDialogStep::Model if self.is_custom_model_selected() => Some(&mut self.model),
            AuthDialogStep::AdvancedConfig => match self.advanced_selected {
                2 => Some(&mut self.context_window_size),
                3 => Some(&mut self.max_tokens),
                _ => None,
            },
            AuthDialogStep::GroupChoice
            | AuthDialogStep::ThirdPartyChoice
            | AuthDialogStep::AuthMethod
            | AuthDialogStep::Protocol
            | AuthDialogStep::CredentialStore
            | AuthDialogStep::Model
            | AuthDialogStep::Review => None,
        }
    }

    fn prepare_custom_model_input(&mut self) -> &mut String {
        let was_custom_model_selected = self.is_custom_model_selected();
        let model_matches_preset = self
            .model_options()
            .iter()
            .any(|model| *model == self.model)
            || self
                .provider
                .and_then(|provider| provider.model)
                .is_some_and(|model| model == self.model);
        self.selected = self.custom_model_index();
        if !was_custom_model_selected || model_matches_preset {
            self.model.clear();
        }
        self.error = None;
        &mut self.model
    }

    fn go_back(&mut self) {
        self.error = None;
        self.review = None;
        self.step = match self.step {
            AuthDialogStep::GroupChoice => AuthDialogStep::GroupChoice,
            AuthDialogStep::ThirdPartyChoice => AuthDialogStep::GroupChoice,
            AuthDialogStep::AuthMethod => match self.provider.map(|provider| provider.source) {
                Some(AuthProviderSource::ThirdParty) => AuthDialogStep::ThirdPartyChoice,
                _ => AuthDialogStep::GroupChoice,
            },
            AuthDialogStep::BaseUrl => match self.provider.map(|provider| provider.source) {
                Some(_) if !self.oauth_methods().is_empty() => AuthDialogStep::AuthMethod,
                Some(AuthProviderSource::Custom) if self.protocol_options().len() > 1 => {
                    AuthDialogStep::Protocol
                }
                Some(AuthProviderSource::ThirdParty) => AuthDialogStep::ThirdPartyChoice,
                _ => AuthDialogStep::GroupChoice,
            },
            AuthDialogStep::CredentialStore => AuthDialogStep::BaseUrl,
            AuthDialogStep::ApiKey => {
                if self.credential_store == AuthCredentialStore::Ephemeral {
                    AuthDialogStep::BaseUrl
                } else {
                    AuthDialogStep::CredentialStore
                }
            }
            AuthDialogStep::EnvKey => AuthDialogStep::CredentialStore,
            AuthDialogStep::Model => {
                if self.credential_store == AuthCredentialStore::Environment {
                    AuthDialogStep::EnvKey
                } else {
                    AuthDialogStep::ApiKey
                }
            }
            AuthDialogStep::AdvancedConfig => AuthDialogStep::Model,
            AuthDialogStep::Review => AuthDialogStep::AdvancedConfig,
            AuthDialogStep::Protocol => AuthDialogStep::GroupChoice,
        };
    }

    fn toggle_advanced_item(&mut self) {
        match self.advanced_selected {
            0 => self.enable_thinking = !self.enable_thinking,
            1 => self.reasoning_effort = next_reasoning_effort(self.reasoning_effort),
            _ => {}
        }
        self.error = None;
    }
}

const AUTH_ADVANCED_ITEMS: usize = 4;
const OPENAI_PROTOCOL_ONLY: &[ProviderProtocol] = &[ProviderProtocol::OpenAiCompatible];
const CUSTOM_PROTOCOL_OPTIONS: &[ProviderProtocol] = &[
    ProviderProtocol::OpenAiCompatible,
    ProviderProtocol::Anthropic,
    ProviderProtocol::Gemini,
    ProviderProtocol::VertexAi,
    ProviderProtocol::Genai,
];
const OFFICIAL_MODELS: &[&str] = &["gpt-test", "gpt-4.1", "qwen-coder-plus"];
const OPENAI_MODELS: &[&str] = &["gpt-5.5", "gpt-5.4", "gpt-4.1"];
const OPENROUTER_MODELS: &[&str] = &[
    "openai/gpt-4.1",
    "anthropic/claude-sonnet-4",
    "qwen/qwen3-coder",
];
const DEEPSEEK_MODELS: &[&str] = &["deepseek-chat", "deepseek-reasoner"];
const QWEN_MODELS: &[&str] = &["qwen-coder-plus", "qwen-plus", "qwen-max"];
const LOCAL_MODELS: &[&str] = &["qwen2.5-coder", "llama3.1", "deepseek-coder"];
const XAI_MODELS: &[&str] = &[
    "grok-4-1-fast-reasoning",
    "grok-4-1-fast-non-reasoning",
    "grok-4",
];
const COPILOT_MODELS: &[&str] = &["gpt-5.5", "gpt-5.3-codex", "gpt-5-mini"];
const CUSTOM_MODELS: &[&str] = &[];

const OFFICIAL_PROVIDER_PRESET: AuthProviderPreset = AuthProviderPreset {
    profile: "golutra",
    title: "Golutra API",
    detail: "Official OpenAI-compatible endpoint",
    source: AuthProviderSource::Official,
    protocol_options: OPENAI_PROTOCOL_ONLY,
    base_url: Some("https://api.golutra.cn/v1"),
    model: Some("gpt-test"),
    recommended_models: OFFICIAL_MODELS,
    oauth_provider_id: None,
    api_key_supported: true,
};

const CUSTOM_PROVIDER_PRESET: AuthProviderPreset = AuthProviderPreset {
    profile: "custom",
    title: "Custom Provider",
    detail: "Manually connect a local server, proxy, or unsupported provider",
    source: AuthProviderSource::Custom,
    protocol_options: CUSTOM_PROTOCOL_OPTIONS,
    base_url: None,
    model: None,
    recommended_models: CUSTOM_MODELS,
    oauth_provider_id: None,
    api_key_supported: true,
};

const THIRD_PARTY_PROVIDER_PRESETS: &[AuthProviderPreset] = &[
    AuthProviderPreset {
        profile: "openai",
        title: "OpenAI",
        detail: "https://api.openai.com/v1",
        source: AuthProviderSource::ThirdParty,
        protocol_options: OPENAI_PROTOCOL_ONLY,
        base_url: Some("https://api.openai.com/v1"),
        model: Some("gpt-5.5"),
        recommended_models: OPENAI_MODELS,
        oauth_provider_id: Some("openai-chatgpt"),
        api_key_supported: true,
    },
    AuthProviderPreset {
        profile: "openrouter",
        title: "OpenRouter",
        detail: "https://openrouter.ai/api/v1",
        source: AuthProviderSource::ThirdParty,
        protocol_options: OPENAI_PROTOCOL_ONLY,
        base_url: Some("https://openrouter.ai/api/v1"),
        model: Some("openai/gpt-4.1"),
        recommended_models: OPENROUTER_MODELS,
        oauth_provider_id: None,
        api_key_supported: true,
    },
    AuthProviderPreset {
        profile: "deepseek",
        title: "DeepSeek",
        detail: "https://api.deepseek.com/v1",
        source: AuthProviderSource::ThirdParty,
        protocol_options: OPENAI_PROTOCOL_ONLY,
        base_url: Some("https://api.deepseek.com/v1"),
        model: Some("deepseek-chat"),
        recommended_models: DEEPSEEK_MODELS,
        oauth_provider_id: None,
        api_key_supported: true,
    },
    AuthProviderPreset {
        profile: "qwen",
        title: "Qwen / DashScope compatible",
        detail: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        source: AuthProviderSource::ThirdParty,
        protocol_options: OPENAI_PROTOCOL_ONLY,
        base_url: Some("https://dashscope.aliyuncs.com/compatible-mode/v1"),
        model: Some("qwen-coder-plus"),
        recommended_models: QWEN_MODELS,
        oauth_provider_id: None,
        api_key_supported: true,
    },
    AuthProviderPreset {
        profile: "xai",
        title: "xAI",
        detail: "SuperGrok OAuth or xAI API key",
        source: AuthProviderSource::ThirdParty,
        protocol_options: OPENAI_PROTOCOL_ONLY,
        base_url: Some("https://api.x.ai/v1"),
        model: Some("grok-4-1-fast-reasoning"),
        recommended_models: XAI_MODELS,
        oauth_provider_id: Some("xai"),
        api_key_supported: true,
    },
    AuthProviderPreset {
        profile: "github-copilot",
        title: "GitHub Copilot",
        detail: "GitHub device authorization",
        source: AuthProviderSource::ThirdParty,
        protocol_options: OPENAI_PROTOCOL_ONLY,
        base_url: Some("https://api.githubcopilot.com/v1"),
        model: Some("gpt-5.5"),
        recommended_models: COPILOT_MODELS,
        oauth_provider_id: Some("github-copilot"),
        api_key_supported: false,
    },
    AuthProviderPreset {
        profile: "local",
        title: "Local OpenAI-compatible",
        detail: "Ollama, LM Studio, vLLM or a local proxy",
        source: AuthProviderSource::ThirdParty,
        protocol_options: OPENAI_PROTOCOL_ONLY,
        base_url: Some("http://localhost:11434/v1"),
        model: Some("qwen2.5-coder"),
        recommended_models: LOCAL_MODELS,
        oauth_provider_id: None,
        api_key_supported: true,
    },
];

const AUTH_GROUP_ITEMS: &[(&str, &str)] = &[
    ("Golutra API", "Official recommended setup with an API key"),
    (
        "Third-party Providers",
        "Choose a known OpenAI-compatible provider",
    ),
    (
        "Custom Provider",
        "Manually connect a local server, proxy, or unsupported provider",
    ),
    ("Continue with mock", "Use local deterministic provider"),
    ("Quit", "Leave without changing provider settings"),
];

#[derive(Debug, Clone, Copy)]
enum ResumeSelectionDirection {
    Previous,
    Next,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TranscriptScrollAction {
    LineUp,
    LineDown,
    PageUp,
    PageDown,
    Top,
    Bottom,
}

impl ResumePickerState {
    fn selected_thread_id(&self) -> Option<ThreadId> {
        self.items.get(self.selected).map(|item| item.thread_id)
    }

    fn move_selection(&mut self, direction: ResumeSelectionDirection) {
        if self.items.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = match direction {
            ResumeSelectionDirection::Previous => self.selected.saturating_sub(1),
            ResumeSelectionDirection::Next => {
                (self.selected + 1).min(self.items.len().saturating_sub(1))
            }
        };
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
    let provider_message = provider_status_message();
    let auth_dialog = initial_auth_dialog();
    let mut terminal = setup_terminal()?;
    let result = run_app(
        &mut terminal,
        TuiApp::new(
            thread_id,
            session_id,
            task_id,
            args.debug,
            provider_message,
            auth_dialog,
        ),
        transport,
    )
    .await;
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
            } else {
                app.debug_mode = !app.debug_mode;
                app.status_message = if app.debug_mode {
                    "debug timeline visible".to_owned()
                } else {
                    "debug timeline hidden".to_owned()
                };
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

async fn handle_auth_dialog_key(
    key: KeyEvent,
    app: &mut TuiApp,
    transport: &RuntimeTransport,
) -> miette::Result<()> {
    match key.code {
        KeyCode::Esc => {
            if let Some(dialog) = &mut app.auth_dialog {
                dialog.go_back();
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if let Some(dialog) = &mut app.auth_dialog {
                if key.code == KeyCode::Up || auth_step_accepts_vim_selection_keys(dialog) {
                    dialog.move_selection(ResumeSelectionDirection::Previous);
                } else if let KeyCode::Char(character) = key.code {
                    handle_auth_dialog_character(dialog, character);
                }
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Some(dialog) = &mut app.auth_dialog {
                if key.code == KeyCode::Down || auth_step_accepts_vim_selection_keys(dialog) {
                    dialog.move_selection(ResumeSelectionDirection::Next);
                } else if let KeyCode::Char(character) = key.code {
                    handle_auth_dialog_character(dialog, character);
                }
            }
        }
        KeyCode::Backspace => {
            if let Some(input) = app
                .auth_dialog
                .as_mut()
                .and_then(AuthDialogState::current_input_mut)
            {
                input.pop();
            }
        }
        KeyCode::Enter => {
            if let Err(error) = advance_auth_dialog(app, transport).await {
                report_auth_dialog_error(app, error);
            }
        }
        KeyCode::Char(character) if character.is_ascii_digit() => {
            if let Some(dialog) = &mut app.auth_dialog
                && matches!(
                    dialog.step,
                    AuthDialogStep::GroupChoice
                        | AuthDialogStep::ThirdPartyChoice
                        | AuthDialogStep::AuthMethod
                        | AuthDialogStep::Protocol
                        | AuthDialogStep::CredentialStore
                )
                && let Some(index) = character
                    .to_digit(10)
                    .and_then(|digit| digit.checked_sub(1))
            {
                let last_index = match dialog.step {
                    AuthDialogStep::GroupChoice => AUTH_GROUP_ITEMS.len().saturating_sub(1),
                    AuthDialogStep::ThirdPartyChoice => {
                        THIRD_PARTY_PROVIDER_PRESETS.len().saturating_sub(1)
                    }
                    AuthDialogStep::AuthMethod => dialog.auth_method_count().saturating_sub(1),
                    AuthDialogStep::Protocol => dialog.protocol_options().len().saturating_sub(1),
                    AuthDialogStep::CredentialStore => 1,
                    AuthDialogStep::BaseUrl
                    | AuthDialogStep::ApiKey
                    | AuthDialogStep::EnvKey
                    | AuthDialogStep::Model
                    | AuthDialogStep::AdvancedConfig
                    | AuthDialogStep::Review => 0,
                };
                if (index as usize) <= last_index {
                    dialog.selected = index as usize;
                    if let Err(error) = advance_auth_dialog(app, transport).await {
                        report_auth_dialog_error(app, error);
                    }
                }
            } else if let Some(dialog) = &mut app.auth_dialog {
                if dialog.step == AuthDialogStep::Model {
                    dialog.prepare_custom_model_input().push(character);
                } else if dialog.step == AuthDialogStep::AdvancedConfig {
                    if let Some(input) = dialog.current_input_mut() {
                        input.push(character);
                    }
                } else if let Some(input) = dialog.current_input_mut() {
                    input.push(character);
                }
            }
        }
        KeyCode::Char(character) => {
            if let Some(dialog) = &mut app.auth_dialog {
                handle_auth_dialog_character(dialog, character);
            }
        }
        _ => {}
    }
    Ok(())
}

fn auth_step_accepts_vim_selection_keys(dialog: &AuthDialogState) -> bool {
    matches!(
        dialog.step,
        AuthDialogStep::GroupChoice
            | AuthDialogStep::ThirdPartyChoice
            | AuthDialogStep::AuthMethod
            | AuthDialogStep::Protocol
            | AuthDialogStep::CredentialStore
            | AuthDialogStep::AdvancedConfig
    )
}

fn handle_auth_dialog_character(dialog: &mut AuthDialogState, character: char) {
    if dialog.step == AuthDialogStep::AdvancedConfig {
        handle_auth_advanced_character(dialog, character);
    } else if dialog.step == AuthDialogStep::Model {
        dialog.prepare_custom_model_input().push(character);
    } else if let Some(input) = dialog.current_input_mut() {
        input.push(character);
    }
}

fn handle_auth_advanced_character(dialog: &mut AuthDialogState, character: char) {
    match character {
        ' ' => dialog.toggle_advanced_item(),
        't' | 'T' => {
            dialog.advanced_selected = 0;
            dialog.toggle_advanced_item();
        }
        'r' | 'R' => {
            dialog.advanced_selected = 1;
            dialog.toggle_advanced_item();
        }
        'c' | 'C' => {
            dialog.advanced_selected = 2;
            dialog.error = None;
        }
        'm' | 'M' => {
            dialog.advanced_selected = 3;
            dialog.error = None;
        }
        character if character.is_ascii_digit() => {
            if let Some(input) = dialog.current_input_mut() {
                input.push(character);
            }
        }
        _ => {}
    }
}

async fn advance_auth_dialog(app: &mut TuiApp, transport: &RuntimeTransport) -> miette::Result<()> {
    let action = {
        let Some(dialog) = &mut app.auth_dialog else {
            return Ok(());
        };
        match dialog.step {
            AuthDialogStep::GroupChoice => match dialog.selected_group_action() {
                AuthGroupAction::Official => {
                    dialog.select_provider(OFFICIAL_PROVIDER_PRESET);
                    AuthAdvanceAction::None
                }
                AuthGroupAction::ThirdParty => {
                    dialog.step = AuthDialogStep::ThirdPartyChoice;
                    dialog.selected = 0;
                    dialog.error = None;
                    AuthAdvanceAction::None
                }
                AuthGroupAction::Custom => {
                    dialog.select_provider(CUSTOM_PROVIDER_PRESET);
                    AuthAdvanceAction::None
                }
                AuthGroupAction::Mock => AuthAdvanceAction::SaveMock,
                AuthGroupAction::Quit => AuthAdvanceAction::Quit,
            },
            AuthDialogStep::ThirdPartyChoice => {
                let provider = dialog.selected_third_party_provider();
                dialog.select_provider(provider);
                AuthAdvanceAction::None
            }
            AuthDialogStep::AuthMethod => {
                if let Some(method) = dialog.selected_oauth_method() {
                    AuthAdvanceAction::StartBuiltinOAuth(Box::new(method))
                } else if dialog.api_key_method_selected() {
                    dialog.step = if dialog.protocol_options().len() > 1 {
                        AuthDialogStep::Protocol
                    } else {
                        AuthDialogStep::BaseUrl
                    };
                    dialog.selected = 0;
                    dialog.error = None;
                    AuthAdvanceAction::None
                } else {
                    dialog.error = Some("No available authentication method".to_owned());
                    AuthAdvanceAction::None
                }
            }
            AuthDialogStep::Protocol => {
                dialog.protocol = dialog.selected_protocol();
                if dialog.base_url.is_empty()
                    && dialog
                        .provider
                        .is_none_or(|provider| provider.source != AuthProviderSource::Custom)
                {
                    dialog.base_url =
                        AuthDialogState::default_base_url_for_protocol(dialog.protocol).to_owned();
                }
                dialog.step = AuthDialogStep::BaseUrl;
                dialog.selected = 0;
                dialog.error = None;
                AuthAdvanceAction::None
            }
            AuthDialogStep::BaseUrl => {
                match validate_auth_base_url(&dialog.base_url) {
                    Ok(base_url) => {
                        dialog.base_url = base_url;
                        dialog.api_key_env = suggested_api_key_env(dialog);
                        dialog.step = if dialog.credential_store == AuthCredentialStore::Ephemeral {
                            AuthDialogStep::ApiKey
                        } else {
                            AuthDialogStep::CredentialStore
                        };
                        dialog.error = None;
                    }
                    Err(error) => {
                        dialog.error = Some(error);
                    }
                }
                AuthAdvanceAction::None
            }
            AuthDialogStep::CredentialStore => {
                dialog.credential_store = if dialog.selected == 1 {
                    AuthCredentialStore::Environment
                } else {
                    AuthCredentialStore::Disk
                };
                if dialog.credential_store == AuthCredentialStore::Environment {
                    dialog.api_key.clear();
                }
                dialog.step = if dialog.credential_store == AuthCredentialStore::Environment {
                    AuthDialogStep::EnvKey
                } else {
                    AuthDialogStep::ApiKey
                };
                dialog.selected = 0;
                dialog.error = None;
                AuthAdvanceAction::None
            }
            AuthDialogStep::ApiKey => {
                if dialog.api_key.trim().is_empty() {
                    dialog.error = Some("API key cannot be empty".to_owned());
                } else {
                    dialog.api_key = dialog.api_key.trim().to_owned();
                    dialog.step = AuthDialogStep::Model;
                    dialog.selected = 0;
                    dialog.error = None;
                }
                AuthAdvanceAction::None
            }
            AuthDialogStep::EnvKey => {
                match CredentialRef::environment(dialog.api_key_env.trim(), SecretKind::ApiKey) {
                    Ok(_) => {
                        dialog.api_key_env = dialog.api_key_env.trim().to_owned();
                        dialog.step = AuthDialogStep::Model;
                        dialog.selected = 0;
                        dialog.error = None;
                    }
                    Err(error) => dialog.error = Some(error.to_string()),
                }
                AuthAdvanceAction::None
            }
            AuthDialogStep::Model => {
                if let Some(model) = dialog.selected_recommended_model() {
                    dialog.model = model.to_owned();
                }
                dialog.model = normalize_model_id(&dialog.model);
                if dialog.model.is_empty() {
                    dialog.error = Some("Model cannot be empty".to_owned());
                    AuthAdvanceAction::None
                } else if !custom_provider_protocol_is_runtime_supported(dialog.protocol) {
                    dialog.error = Some(format!(
                        "{} setup is recognized, but Golutra live runtime currently only supports OpenAI-compatible providers",
                        protocol_option_text(dialog.protocol).0
                    ));
                    AuthAdvanceAction::None
                } else {
                    dialog.step = AuthDialogStep::AdvancedConfig;
                    dialog.error = None;
                    AuthAdvanceAction::None
                }
            }
            AuthDialogStep::AdvancedConfig => {
                match validate_generation_config(dialog).and_then(|_| build_auth_review(dialog)) {
                    Ok(review) => {
                        dialog.review = Some(review);
                        dialog.step = AuthDialogStep::Review;
                        dialog.error = None;
                    }
                    Err(error) => {
                        dialog.error = Some(error);
                    }
                }
                AuthAdvanceAction::None
            }
            AuthDialogStep::Review => match auth_login(dialog) {
                Ok(login) => AuthAdvanceAction::SaveOpenAiCompatible(Box::new(login)),
                Err(error) => {
                    dialog.error = Some(error);
                    AuthAdvanceAction::None
                }
            },
        }
    };
    match action {
        AuthAdvanceAction::None => {}
        AuthAdvanceAction::SaveMock => {
            apply_auth_mock()?;
            app.provider_message = provider_status_message();
            app.auth_dialog = None;
            app.status_message = "using mock provider".to_owned();
        }
        AuthAdvanceAction::SaveOpenAiCompatible(login) => {
            apply_auth_login(transport, *login).await?;
            app.provider_message = provider_status_message();
            app.auth_dialog = None;
            app.status_message = "provider connected".to_owned();
        }
        AuthAdvanceAction::StartBuiltinOAuth(method) => {
            app.auth_dialog = None;
            app.start_builtin_oauth_login(transport, *method)?;
        }
        AuthAdvanceAction::Quit => app.should_quit = true,
    }
    Ok(())
}

fn report_auth_dialog_error(app: &mut TuiApp, error: miette::Report) {
    let message = error.to_string();
    if let Some(dialog) = &mut app.auth_dialog {
        dialog.error = Some(message);
    }
    app.status_message = "provider setup failed".to_owned();
}

fn custom_provider_protocol_is_runtime_supported(protocol: ProviderProtocol) -> bool {
    provider_protocol_has_runtime_adapter(protocol)
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

fn draw_ui(frame: &mut Frame<'_>, app: &TuiApp) {
    let bottom_height = bottom_pane_height(app);
    let constraints = if app.debug_mode {
        vec![
            Constraint::Length(1),
            Constraint::Min(8),
            Constraint::Length(8),
            Constraint::Length(bottom_height),
        ]
    } else {
        vec![
            Constraint::Length(1),
            Constraint::Min(8),
            Constraint::Length(bottom_height),
        ]
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(frame.area());

    draw_header(frame, chunks[0], app);
    draw_transcript(frame, chunks[1], app);
    if app.debug_mode {
        draw_debug_timeline(frame, chunks[2], app);
        draw_bottom_pane(frame, chunks[3], app);
    } else {
        draw_bottom_pane(frame, chunks[2], app);
    }
}

fn bottom_pane_height(app: &TuiApp) -> u16 {
    let slash_rows = app.slash_candidates().len() as u16;
    if app.auth_dialog.is_some()
        || app.resume_picker.is_some()
        || provider_footer_line(app).is_some()
    {
        4 + slash_rows
    } else {
        3 + slash_rows
    }
}

fn draw_header(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let mode = header_mode(app);
    let lines = vec![Line::from(vec![
        Span::styled(
            "Golutra",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(mode, Style::default().fg(Color::DarkGray)),
    ])];
    let paragraph = Paragraph::new(lines).alignment(Alignment::Left);
    frame.render_widget(paragraph, area);
}

fn header_mode(app: &TuiApp) -> String {
    if app.auth_dialog.is_some() {
        return "  auth".to_owned();
    }
    if app.resume_picker.is_some() {
        return "  resume".to_owned();
    }
    if app.debug_mode {
        return "  debug".to_owned();
    }
    match app.projection.as_ref().map(|projection| projection.status) {
        Some(golutra_core::TaskStatus::Running) => "  running".to_owned(),
        Some(golutra_core::TaskStatus::WaitingApproval) => "  waiting".to_owned(),
        Some(golutra_core::TaskStatus::Failed) => "  failed".to_owned(),
        Some(golutra_core::TaskStatus::Blocked) => "  blocked".to_owned(),
        Some(golutra_core::TaskStatus::Completed) => "  complete".to_owned(),
        _ => String::new(),
    }
}

fn draw_transcript(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    if let Some(dialog) = &app.auth_dialog {
        draw_auth_dialog(frame, area, dialog);
        return;
    }
    if let Some(picker) = &app.resume_picker {
        draw_resume_picker(frame, area, picker, app.thread_id);
        return;
    }

    let mut items = transcript_rows(app);
    if items.is_empty() {
        return;
    }
    let visible_rows = area.height.saturating_sub(1) as usize;
    let window = transcript_visible_window(items.len(), visible_rows, app.transcript_scroll_offset);
    if window.end < items.len() {
        items.drain(window.end..);
    }
    if window.start > 0 {
        items.drain(..window.start);
    }
    let list = List::new(items).block(Block::default().borders(Borders::TOP));
    frame.render_widget(list, area);
}

fn draw_auth_dialog(frame: &mut Frame<'_>, area: Rect, dialog: &AuthDialogState) {
    let lines = match dialog.step {
        AuthDialogStep::GroupChoice => auth_group_lines(dialog),
        AuthDialogStep::ThirdPartyChoice => auth_third_party_lines(dialog),
        AuthDialogStep::AuthMethod => auth_method_lines(dialog),
        AuthDialogStep::Protocol => auth_protocol_lines(dialog),
        AuthDialogStep::BaseUrl => auth_input_lines(
            &auth_step_title(dialog),
            "Base URL",
            "endpoint URL for the selected protocol",
            &dialog.base_url,
            dialog.error.as_deref(),
            false,
        ),
        AuthDialogStep::CredentialStore => auth_credential_store_lines(dialog),
        AuthDialogStep::ApiKey => auth_input_lines(
            &auth_step_title(dialog),
            "API key",
            "stored in $GOLUTRA_HOME/credentials.json",
            &dialog.api_key,
            dialog.error.as_deref(),
            true,
        ),
        AuthDialogStep::EnvKey => auth_input_lines(
            &auth_step_title(dialog),
            "Environment variable",
            "for example OPENAI_API_KEY",
            &dialog.api_key_env,
            dialog.error.as_deref(),
            false,
        ),
        AuthDialogStep::Model => auth_model_lines(dialog),
        AuthDialogStep::AdvancedConfig => auth_advanced_config_lines(dialog),
        AuthDialogStep::Review => auth_review_lines(dialog),
    };
    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .title("Provider setup")
                .borders(Borders::TOP),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn auth_group_lines(dialog: &AuthDialogState) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![Span::styled(
        "Connect a Provider",
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )])];
    lines.push(Line::from(""));
    lines.extend(
        AUTH_GROUP_ITEMS
            .iter()
            .enumerate()
            .map(|(index, (title, detail))| {
                auth_option_line(index, title, detail, index == dialog.selected)
            }),
    );
    push_auth_error(&mut lines, dialog.error.as_deref());
    lines
}

fn auth_third_party_lines(dialog: &AuthDialogState) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![Span::styled(
        "Third-party Providers",
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )])];
    lines.push(Line::from(""));
    lines.extend(
        THIRD_PARTY_PROVIDER_PRESETS
            .iter()
            .enumerate()
            .map(|(index, preset)| {
                auth_option_line(index, preset.title, preset.detail, index == dialog.selected)
            }),
    );
    push_auth_error(&mut lines, dialog.error.as_deref());
    lines
}

fn auth_method_lines(dialog: &AuthDialogState) -> Vec<Line<'static>> {
    let provider = dialog
        .provider
        .map(|provider| provider.title)
        .unwrap_or("Provider");
    let mut lines = vec![Line::from(vec![Span::styled(
        format!("{provider} authentication"),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )])];
    lines.push(Line::from(""));
    let methods = dialog.oauth_methods();
    lines.extend(methods.iter().enumerate().map(|(index, method)| {
        let detail = match method.flow {
            OAuthFlow::BrowserPkce => "Open browser and complete PKCE authorization",
            OAuthFlow::DeviceCode => "Open a verification page and enter a device code",
            OAuthFlow::OpenAiDeviceAuth => "Open the ChatGPT device page and enter a code",
        };
        auth_option_line(index, &method.label, detail, index == dialog.selected)
    }));
    if dialog
        .provider
        .is_some_and(|provider| provider.api_key_supported)
    {
        lines.push(auth_option_line(
            methods.len(),
            "API key",
            "Store a key on local disk or reference an environment variable",
            dialog.selected >= methods.len(),
        ));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Enter to select, Up/Down to navigate, Esc to go back",
        Style::default().fg(Color::DarkGray),
    )));
    push_auth_error(&mut lines, dialog.error.as_deref());
    lines
}

fn auth_protocol_lines(dialog: &AuthDialogState) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![Span::styled(
        auth_step_title(dialog),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )])];
    lines.push(Line::from(""));
    lines.extend(
        dialog
            .protocol_options()
            .iter()
            .enumerate()
            .map(|(index, protocol)| {
                let (title, detail) = protocol_option_text(*protocol);
                auth_option_line(index, title, detail, index == dialog.selected)
            }),
    );
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Enter to select, ↑↓ to navigate, Esc to go back",
        Style::default().fg(Color::DarkGray),
    )));
    push_auth_error(&mut lines, dialog.error.as_deref());
    lines
}

fn auth_credential_store_lines(dialog: &AuthDialogState) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![Span::styled(
        auth_step_title(dialog),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )])];
    lines.push(Line::from(""));
    lines.push(auth_option_line(
        0,
        "Local disk",
        "Store in $GOLUTRA_HOME/credentials.json (owner-only)",
        dialog.selected == 0,
    ));
    lines.push(auth_option_line(
        1,
        "Environment variable",
        "Store only a read-only env reference for CI or managed shells",
        dialog.selected == 1,
    ));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Enter to select, Up/Down to navigate, Esc to go back",
        Style::default().fg(Color::DarkGray),
    )));
    push_auth_error(&mut lines, dialog.error.as_deref());
    lines
}

fn protocol_option_text(protocol: ProviderProtocol) -> (&'static str, &'static str) {
    match protocol {
        ProviderProtocol::OpenAiCompatible => (
            "OpenAI-compatible",
            "Standard OpenAI API format (most common)",
        ),
        ProviderProtocol::Anthropic => ("Anthropic-compatible", "Anthropic Messages API format"),
        ProviderProtocol::Gemini => ("Gemini-compatible", "Google Gemini API format"),
        ProviderProtocol::VertexAi => (
            "Vertex AI",
            "Google Cloud project/location endpoint with OAuth token",
        ),
        ProviderProtocol::Genai => ("rust-genai", "Model-routed native provider adapter"),
        _ => ("Unsupported", "Not available for custom provider setup"),
    }
}

fn auth_model_lines(dialog: &AuthDialogState) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![Span::styled(
        auth_step_title(dialog),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )])];
    lines.push(Line::from(""));
    if dialog.model_options().is_empty() {
        lines.push(Line::from(vec![
            Span::styled("Model: ", Style::default().fg(Color::Cyan)),
            Span::styled(
                if dialog.model.is_empty() {
                    "model id, for example gpt-4.1 or qwen-coder".to_owned()
                } else {
                    dialog.model.clone()
                },
                Style::default().fg(if dialog.model.is_empty() {
                    Color::DarkGray
                } else {
                    Color::White
                }),
            ),
        ]));
    } else {
        lines.push(Line::from(Span::styled(
            "Recommended models",
            Style::default().fg(Color::DarkGray),
        )));
        lines.extend(
            dialog
                .model_options()
                .iter()
                .enumerate()
                .map(|(index, model)| {
                    auth_option_line(
                        index,
                        model,
                        "built-in recommendation",
                        index == dialog.selected,
                    )
                }),
        );
        let custom_value = if dialog.model.is_empty() {
            "type a custom model id"
        } else {
            dialog.model.as_str()
        };
        lines.push(auth_option_line(
            dialog.custom_model_index(),
            "Custom model",
            custom_value,
            dialog.is_custom_model_selected(),
        ));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Enter continue   Type to use custom model   Esc back",
        Style::default().fg(Color::DarkGray),
    )));
    push_auth_error(&mut lines, dialog.error.as_deref());
    lines
}

fn auth_advanced_config_lines(dialog: &AuthDialogState) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(vec![Span::styled(
            auth_step_title(dialog),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::from(""),
        auth_option_line(
            0,
            "Thinking",
            if dialog.enable_thinking {
                "enabled"
            } else {
                "default"
            },
            dialog.advanced_selected == 0,
        ),
        auth_option_line(
            1,
            "Reasoning effort",
            reasoning_effort_label(dialog.reasoning_effort),
            dialog.advanced_selected == 1,
        ),
        auth_option_line(
            2,
            "Context window",
            if dialog.context_window_size.is_empty() {
                "default"
            } else {
                dialog.context_window_size.as_str()
            },
            dialog.advanced_selected == 2,
        ),
        auth_option_line(
            3,
            "Max output tokens",
            if dialog.max_tokens.is_empty() {
                "default"
            } else {
                dialog.max_tokens.as_str()
            },
            dialog.advanced_selected == 3,
        ),
        Line::from(""),
        Line::from(Span::styled(
            "Up/Down select   Space toggle/cycle   Type digits for numeric fields   Enter continue   Esc back",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    push_auth_error(&mut lines, dialog.error.as_deref());
    lines
}

fn auth_review_lines(dialog: &AuthDialogState) -> Vec<Line<'static>> {
    let Some(review) = &dialog.review else {
        return vec![Line::from(Span::styled(
            "Review is not ready",
            Style::default().fg(Color::Red),
        ))];
    };
    let update_line = if review.updates_existing_profile {
        "will update existing profile"
    } else {
        "will create new profile"
    };
    let mut lines = vec![
        Line::from(vec![Span::styled(
            "Review provider setup",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::from(""),
        auth_kv_line("Provider", review.provider_title),
        auth_kv_line("Profile", &review.profile),
        auth_kv_line("Protocol", &review.protocol),
        auth_kv_line("Base URL", &review.base_url),
        auth_kv_line("Model", &review.model),
        auth_kv_line("Credential", &review.credential),
        auth_kv_line("Advanced", &review.advanced),
        auth_kv_line("Scope", provider_scope_label(review.scope)),
        auth_kv_line("Config", &review.config_path.display().to_string()),
        auth_kv_line("Plan", update_line),
        Line::from(""),
        Line::from(Span::styled(
            "Install plan preview",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    lines.extend(review.preview_json.lines().take(8).map(|line| {
        Line::from(Span::styled(
            line.to_owned(),
            Style::default().fg(Color::Gray),
        ))
    }));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Enter save   Esc back   Ctrl+C twice quit",
        Style::default().fg(Color::DarkGray),
    )));
    push_auth_error(&mut lines, dialog.error.as_deref());
    lines
}

fn auth_kv_line(label: &'static str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label}: "), Style::default().fg(Color::Cyan)),
        Span::styled(value.to_owned(), Style::default().fg(Color::White)),
    ])
}

fn provider_scope_label(scope: ProviderConfigScope) -> &'static str {
    match scope {
        ProviderConfigScope::User => "user",
        ProviderConfigScope::Workspace => "workspace",
    }
}

fn auth_option_line(index: usize, title: &str, detail: &str, selected: bool) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            if selected { "> " } else { "  " },
            Style::default().fg(if selected {
                Color::Cyan
            } else {
                Color::DarkGray
            }),
        ),
        Span::styled(
            format!("{} ", index + 1),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            title.to_owned(),
            Style::default()
                .fg(if selected { Color::White } else { Color::Gray })
                .add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ),
        Span::raw("  "),
        Span::styled(detail.to_owned(), Style::default().fg(Color::DarkGray)),
    ])
}

fn push_auth_error(lines: &mut Vec<Line<'static>>, error: Option<&str>) {
    if let Some(error) = error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            error.to_owned(),
            Style::default().fg(Color::Red),
        )));
    }
}

fn auth_step_title(dialog: &AuthDialogState) -> String {
    let provider_title = dialog
        .provider
        .map(|provider| provider.title)
        .unwrap_or("Connect provider");
    if matches!(
        dialog.provider.map(|provider| provider.source),
        Some(AuthProviderSource::Custom)
    ) {
        let step = match dialog.step {
            AuthDialogStep::Protocol => "Step 1/7 · Protocol",
            AuthDialogStep::BaseUrl => "Step 2/7 · Base URL",
            AuthDialogStep::CredentialStore => "Step 3/7 · Credential storage",
            AuthDialogStep::ApiKey => "Step 4/7 · API Key",
            AuthDialogStep::EnvKey => "Step 4/7 · Environment variable",
            AuthDialogStep::Model => "Step 5/7 · Model IDs",
            AuthDialogStep::AdvancedConfig => "Step 6/7 · Advanced Config",
            AuthDialogStep::Review => "Step 7/7 · Review",
            AuthDialogStep::GroupChoice
            | AuthDialogStep::ThirdPartyChoice
            | AuthDialogStep::AuthMethod => "",
        };
        if step.is_empty() {
            provider_title.to_owned()
        } else {
            format!("{provider_title} · {step}")
        }
    } else {
        provider_title.to_owned()
    }
}

fn auth_input_lines(
    title: &str,
    label: &'static str,
    hint: &'static str,
    value: &str,
    error: Option<&str>,
    secret: bool,
) -> Vec<Line<'static>> {
    let visible_value = if secret && !value.is_empty() {
        "*".repeat(value.chars().count())
    } else {
        value.to_owned()
    };
    let input = if visible_value.is_empty() {
        hint.to_owned()
    } else {
        visible_value
    };
    let mut lines = vec![
        Line::from(vec![Span::styled(
            title.to_owned(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::from(""),
        Line::from(vec![
            Span::styled(format!("{label}: "), Style::default().fg(Color::Cyan)),
            Span::styled(
                input,
                Style::default().fg(if value.is_empty() {
                    Color::DarkGray
                } else {
                    Color::White
                }),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "Enter continue   Esc back   Ctrl+C twice quit",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    if let Some(error) = error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            error.to_owned(),
            Style::default().fg(Color::Red),
        )));
    }
    lines
}

fn draw_resume_picker(
    frame: &mut Frame<'_>,
    area: Rect,
    picker: &ResumePickerState,
    current_thread_id: ThreadId,
) {
    let visible_rows = area.height.saturating_sub(1) as usize;
    let visible_count = visible_rows.max(1);
    let offset = resume_picker_offset(picker.selected, visible_count, picker.items.len());
    let items = picker
        .items
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible_count)
        .map(|(index, item)| {
            let selected = index == picker.selected;
            let current = item.thread_id == current_thread_id;
            let marker = if selected { "> " } else { "  " };
            let current_marker = if current { "current" } else { "" };
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(
                        marker,
                        Style::default().fg(if selected {
                            Color::Cyan
                        } else {
                            Color::DarkGray
                        }),
                    ),
                    Span::styled(
                        format!("{} ", index + 1),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(
                        item.title.clone(),
                        Style::default()
                            .fg(if selected { Color::White } else { Color::Gray })
                            .add_modifier(if selected {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ),
                    Span::raw("  "),
                    Span::styled(current_marker, Style::default().fg(Color::Green)),
                ]),
                Line::from(vec![
                    Span::raw("    "),
                    Span::styled(
                        short_id(&item.session_id.to_string()),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::raw("  "),
                    Span::styled(item.preview.clone(), Style::default().fg(Color::DarkGray)),
                ]),
            ])
        })
        .collect::<Vec<_>>();
    let list = List::new(items).block(
        Block::default()
            .title("Resume session")
            .borders(Borders::TOP),
    );
    frame.render_widget(list, area);
}

fn resume_picker_offset(selected: usize, visible_count: usize, item_count: usize) -> usize {
    if visible_count == 0 || item_count <= visible_count || selected < visible_count {
        return 0;
    }
    let last_window_start = item_count.saturating_sub(visible_count);
    selected
        .saturating_add(1)
        .saturating_sub(visible_count)
        .min(last_window_start)
}

fn draw_debug_timeline(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let mut lines = event_timeline_lines(&app.events);
    lines.truncate(6);

    let items = if lines.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "no runtime events yet",
            Style::default().fg(Color::DarkGray),
        )))]
    } else {
        lines
            .into_iter()
            .rev()
            .map(|line| {
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("#{} ", line.sequence_no),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(line.label, Style::default().fg(Color::Yellow)),
                    Span::styled("  ", Style::default()),
                    Span::raw(line.summary),
                ]))
            })
            .collect()
    };
    let list = List::new(items).block(
        Block::default()
            .title("Debug timeline")
            .borders(Borders::TOP),
    );
    frame.render_widget(list, area);
}

fn draw_bottom_pane(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let help = if app.auth_dialog.is_some() {
        "Provider setup   Enter continue   Esc back   Ctrl+C twice quit"
    } else if app.resume_picker.is_some() {
        "Enter resume   Up/Down select   Esc cancel   Ctrl+C twice quit"
    } else {
        "Enter send   PgUp/PgDn history   Home/End jump   Ctrl+C interrupt"
    };
    let candidates = app.slash_candidates();
    let help_line = help.to_owned();
    let input_line = if let Some(dialog) = &app.auth_dialog {
        auth_composer_line(dialog)
    } else if app.resume_picker.is_some() {
        "Select a session to resume".to_owned()
    } else if app.input.is_empty() {
        "Ask Golutra to change code or inspect the workspace".to_owned()
    } else {
        app.input.clone()
    };
    let mut lines = vec![Line::from(vec![
        Span::styled("› ", Style::default().fg(Color::Cyan)),
        Span::styled(input_line, composer_style(app)),
    ])];
    lines.extend(slash_candidate_lines(app, &candidates));
    lines.push(footer_status_line(app));
    if let Some(provider_line) = provider_footer_line(app) {
        lines.push(Line::from(Span::styled(
            provider_line,
            Style::default().fg(provider_color(app)),
        )));
    }
    lines.push(Line::from(Span::styled(
        help_line,
        Style::default().fg(Color::DarkGray),
    )));
    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::TOP))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn slash_candidate_lines(app: &TuiApp, candidates: &[SlashCommandCandidate]) -> Vec<Line<'static>> {
    candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| {
            let selected = index == app.slash_selected.min(candidates.len().saturating_sub(1));
            let marker = if selected { "› " } else { "  " };
            Line::from(vec![
                Span::styled(
                    marker,
                    Style::default().fg(if selected {
                        Color::Cyan
                    } else {
                        Color::DarkGray
                    }),
                ),
                Span::styled(
                    candidate.command.clone(),
                    Style::default()
                        .fg(if selected { Color::White } else { Color::Gray })
                        .add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Span::raw("  "),
                Span::styled(
                    candidate.description.clone(),
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        })
        .collect()
}

fn auth_composer_line(dialog: &AuthDialogState) -> String {
    match dialog.step {
        AuthDialogStep::GroupChoice => "Select provider group".to_owned(),
        AuthDialogStep::ThirdPartyChoice => "Select provider".to_owned(),
        AuthDialogStep::AuthMethod => "Select authentication method".to_owned(),
        AuthDialogStep::Protocol => "Select protocol".to_owned(),
        AuthDialogStep::BaseUrl if dialog.base_url.is_empty() => "Base URL".to_owned(),
        AuthDialogStep::BaseUrl => dialog.base_url.clone(),
        AuthDialogStep::CredentialStore => match dialog.selected {
            1 => "Environment variable".to_owned(),
            _ => "Local disk".to_owned(),
        },
        AuthDialogStep::ApiKey if dialog.api_key.is_empty() => "API key".to_owned(),
        AuthDialogStep::ApiKey => "*".repeat(dialog.api_key.chars().count()),
        AuthDialogStep::EnvKey if dialog.api_key_env.is_empty() => {
            "Environment variable".to_owned()
        }
        AuthDialogStep::EnvKey => dialog.api_key_env.clone(),
        AuthDialogStep::Model if dialog.is_custom_model_selected() && !dialog.model.is_empty() => {
            dialog.model.clone()
        }
        AuthDialogStep::Model => dialog
            .selected_recommended_model()
            .unwrap_or("Custom model")
            .to_owned(),
        AuthDialogStep::AdvancedConfig => match dialog.advanced_selected {
            0 => format!(
                "Thinking: {}",
                if dialog.enable_thinking {
                    "enabled"
                } else {
                    "default"
                }
            ),
            1 => format!(
                "Reasoning effort: {}",
                reasoning_effort_label(dialog.reasoning_effort)
            ),
            2 => {
                if dialog.context_window_size.is_empty() {
                    "Context window".to_owned()
                } else {
                    dialog.context_window_size.clone()
                }
            }
            3 => {
                if dialog.max_tokens.is_empty() {
                    "Max output tokens".to_owned()
                } else {
                    dialog.max_tokens.clone()
                }
            }
            _ => "Advanced config".to_owned(),
        },
        AuthDialogStep::Review => "Review install plan".to_owned(),
    }
}

fn footer_status_line(app: &TuiApp) -> Line<'static> {
    let detail = footer_status_detail(app);
    let spans = if detail.is_empty() {
        vec![Span::styled(
            status_chip(app),
            Style::default().fg(status_color(app)),
        )]
    } else {
        vec![
            Span::styled(status_chip(app), Style::default().fg(status_color(app))),
            Span::raw("  "),
            Span::styled(detail, Style::default().fg(Color::DarkGray)),
        ]
    };
    Line::from(spans)
}

fn footer_status_detail(app: &TuiApp) -> String {
    if app.auth_dialog.is_some() {
        return "connect provider".to_owned();
    }
    if app.status_message.trim().is_empty() {
        return match app.projection.as_ref().map(|projection| projection.status) {
            Some(golutra_core::TaskStatus::Idle) | None => "new session".to_owned(),
            _ => String::new(),
        };
    }
    if matches!(
        app.projection.as_ref().map(|projection| projection.status),
        Some(golutra_core::TaskStatus::Running) | Some(golutra_core::TaskStatus::Completed)
    ) && app.status_message == "task started"
    {
        String::new()
    } else {
        app.status_message.clone()
    }
}

fn provider_footer_line(app: &TuiApp) -> Option<String> {
    if app.auth_dialog.is_some() {
        return None;
    }
    if app.provider_message == "ready (mock)" || app.provider_message.starts_with("ready (") {
        None
    } else {
        Some(app.provider_message.clone())
    }
}

fn transcript_items(app: &TuiApp) -> Vec<TranscriptItem> {
    if app.auth_dialog.is_some() {
        return Vec::new();
    }
    let mut items = Vec::new();
    let event_items = event_transcript_items(&app.events);
    let has_event_items = !event_items.is_empty();
    items.extend(event_items);
    items.extend(app.command_messages.clone());
    if let Some(projection) = &app.projection {
        if has_event_items {
            items.extend(projection_overlay_items(projection));
        } else {
            items.extend(projection_items(projection));
        }
    } else {
        items.push(TranscriptItem {
            role: TranscriptRole::System,
            title: "Connecting".to_owned(),
            body: vec!["loading runtime state".to_owned()],
        });
    }

    items
}

fn transcript_rows(app: &TuiApp) -> Vec<ListItem<'static>> {
    transcript_items(app)
        .into_iter()
        .flat_map(transcript_list_items)
        .collect()
}

fn transcript_visible_window(
    total_rows: usize,
    visible_rows: usize,
    scroll_offset: usize,
) -> std::ops::Range<usize> {
    if total_rows == 0 || visible_rows == 0 {
        return 0..0;
    }
    let visible_rows = visible_rows.min(total_rows);
    let max_offset = total_rows.saturating_sub(visible_rows);
    let offset = scroll_offset.min(max_offset);
    let end = total_rows.saturating_sub(offset);
    end.saturating_sub(visible_rows)..end
}

fn transcript_page_rows(app: &TuiApp) -> usize {
    let terminal_height = size().map(|(_, height)| height).unwrap_or(24);
    usize::from(
        terminal_height
            .saturating_sub(1)
            .saturating_sub(bottom_pane_height(app))
            .saturating_sub(1)
            .max(1),
    )
}

fn transcript_scroll_status(scroll_offset: usize) -> String {
    if scroll_offset == 0 {
        "history at latest".to_owned()
    } else {
        format!("history offset {scroll_offset} rows from latest")
    }
}

fn event_transcript_items(events: &[Value]) -> Vec<TranscriptItem> {
    let mut typed_events = events
        .iter()
        .filter_map(|value| serde_json::from_value::<RuntimeEvent>(value.clone()).ok())
        .collect::<Vec<_>>();
    typed_events.sort_by_key(|event| event.sequence_no);

    let mut items = Vec::new();
    let mut visible_user_turns = HashSet::new();
    for event in typed_events {
        match event.event_type {
            RuntimeEventType::TaskCreated | RuntimeEventType::TurnQueued => {
                let is_new_turn = event
                    .turn_id
                    .is_none_or(|turn_id| visible_user_turns.insert(turn_id));
                if is_new_turn && let Some(item) = user_event_transcript_item(&event) {
                    items.push(item);
                }
            }
            RuntimeEventType::AssistantMessage => {
                if let Some(item) = assistant_event_transcript_item(&event) {
                    items.push(item);
                }
            }
            _ => {
                if let Some(item) = status_event_transcript_item(&event) {
                    items.push(item);
                }
            }
        }
    }
    items
}

fn user_event_transcript_item(event: &RuntimeEvent) -> Option<TranscriptItem> {
    event
        .payload
        .get("payload")
        .and_then(|payload| payload.get("prompt"))
        .and_then(Value::as_str)
        .filter(|prompt| !prompt.trim().is_empty())
        .map(|prompt| TranscriptItem {
            role: TranscriptRole::User,
            title: "You".to_owned(),
            body: vec![prompt.to_owned()],
        })
}

fn assistant_event_transcript_item(event: &RuntimeEvent) -> Option<TranscriptItem> {
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

fn status_event_transcript_item(event: &RuntimeEvent) -> Option<TranscriptItem> {
    if event.event_type == RuntimeEventType::ApprovalRequested {
        let request = event.payload.get("request")?;
        let tool_name = request
            .get("tool_name")
            .and_then(Value::as_str)
            .unwrap_or("tool");
        let resource = request
            .get("resource")
            .and_then(Value::as_str)
            .unwrap_or("unknown resource");
        let reason = request
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("explicit approval is required");
        return Some(TranscriptItem {
            role: TranscriptRole::Status,
            title: "Approval required".to_owned(),
            body: vec![format!("{tool_name}: {resource}"), reason.to_owned()],
        });
    }
    if event.event_type == RuntimeEventType::TaskCompleted
        && event
            .payload
            .get("status")
            .cloned()
            .and_then(|status| serde_json::from_value::<golutra_core::TaskStatus>(status).ok())
            == Some(golutra_core::TaskStatus::Completed)
    {
        return None;
    }
    let title = event_status_title(event.event_type)?;
    let summary = event_summary(event)?;
    if event.event_type == RuntimeEventType::LoopDecided
        && !summary.contains("failed")
        && !summary.contains("error")
    {
        return None;
    }
    Some(TranscriptItem {
        role: TranscriptRole::Status,
        title: title.to_owned(),
        body: vec![summary],
    })
}

fn event_status_title(event_type: RuntimeEventType) -> Option<&'static str> {
    match event_type {
        RuntimeEventType::ToolCompleted => Some("Tool Completed"),
        RuntimeEventType::TaskCompleted => Some("Task Completed"),
        RuntimeEventType::CommandRejected => Some("Command Rejected"),
        RuntimeEventType::ControllerChanged => Some("Controller Changed"),
        RuntimeEventType::LoopDecided => Some("Loop Decided"),
        _ => None,
    }
}

fn event_summary(event: &RuntimeEvent) -> Option<String> {
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

fn projection_items(projection: &UserProjection) -> Vec<TranscriptItem> {
    let mut items = projection
        .visible_steps
        .iter()
        .filter(|step| significant_step(step))
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

    if !projection.residual_risks.is_empty() {
        items.push(TranscriptItem {
            role: TranscriptRole::Status,
            title: "Residual risks".to_owned(),
            body: projection.residual_risks.clone(),
        });
    }
    items
}

fn projection_overlay_items(projection: &UserProjection) -> Vec<TranscriptItem> {
    let mut items = Vec::new();
    if !projection.residual_risks.is_empty() {
        items.push(TranscriptItem {
            role: TranscriptRole::Status,
            title: "Residual risks".to_owned(),
            body: projection.residual_risks.clone(),
        });
    }
    items
}

fn significant_step(step: &VisibleStep) -> bool {
    matches!(step.label.as_str(), "ToolCompleted" | "CommandRejected")
        || (step.label == "TaskCompleted" && step.status != "Completed")
        || (step.label == "LoopDecided"
            && (step.summary.contains("failed") || step.summary.contains("error")))
}

fn step_item(step: &VisibleStep) -> TranscriptItem {
    TranscriptItem {
        role: TranscriptRole::Status,
        title: readable_step_label(&step.label),
        body: vec![format!("{} - {}", step.status, step.summary)],
    }
}

fn transcript_list_items(item: TranscriptItem) -> Vec<ListItem<'static>> {
    let color = role_color(&item.role);
    let mut rows = vec![ListItem::new(Line::from(vec![
        Span::styled(role_marker(&item.role), Style::default().fg(color)),
        Span::styled(
            item.title,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ]))];
    rows.extend(item.body.into_iter().map(|line| {
        ListItem::new(Line::from(vec![
            Span::raw("  "),
            Span::styled(line, Style::default().fg(Color::White)),
        ]))
    }));
    rows.push(ListItem::new(Line::from("")));
    rows
}

fn readable_step_label(label: &str) -> String {
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

fn role_marker(role: &TranscriptRole) -> &'static str {
    match role {
        TranscriptRole::User => "› ",
        TranscriptRole::Assistant | TranscriptRole::Status | TranscriptRole::System => "• ",
    }
}

fn role_color(role: &TranscriptRole) -> Color {
    match role {
        TranscriptRole::User => Color::Cyan,
        TranscriptRole::Assistant => Color::Green,
        TranscriptRole::Status => Color::Yellow,
        TranscriptRole::System => Color::DarkGray,
    }
}

fn status_chip(app: &TuiApp) -> &'static str {
    if app.auth_dialog.is_some() {
        return "auth";
    }
    match app.projection.as_ref().map(|projection| projection.status) {
        Some(golutra_core::TaskStatus::Running) => "running",
        Some(golutra_core::TaskStatus::WaitingApproval) => "waiting approval",
        Some(golutra_core::TaskStatus::Completed) => "complete",
        Some(golutra_core::TaskStatus::Failed) => "failed",
        Some(golutra_core::TaskStatus::Blocked) => "blocked",
        Some(golutra_core::TaskStatus::Cancelled) => "cancelled",
        Some(golutra_core::TaskStatus::Aborting) => "aborting",
        Some(golutra_core::TaskStatus::Paused) => "paused",
        Some(golutra_core::TaskStatus::Pausing) => "pausing",
        Some(golutra_core::TaskStatus::Partial) => "partial",
        Some(golutra_core::TaskStatus::Idle) | None => "ready",
    }
}

fn status_color(app: &TuiApp) -> Color {
    if app.auth_dialog.is_some() {
        return Color::Cyan;
    }
    match app.projection.as_ref().map(|projection| projection.status) {
        Some(golutra_core::TaskStatus::Running) => Color::Cyan,
        Some(golutra_core::TaskStatus::Completed) => Color::Green,
        Some(golutra_core::TaskStatus::Failed)
        | Some(golutra_core::TaskStatus::Blocked)
        | Some(golutra_core::TaskStatus::Cancelled) => Color::Red,
        Some(golutra_core::TaskStatus::WaitingApproval)
        | Some(golutra_core::TaskStatus::Aborting)
        | Some(golutra_core::TaskStatus::Pausing)
        | Some(golutra_core::TaskStatus::Partial) => Color::Yellow,
        Some(golutra_core::TaskStatus::Paused) => Color::Magenta,
        Some(golutra_core::TaskStatus::Idle) | None => Color::DarkGray,
    }
}

fn provider_color(app: &TuiApp) -> Color {
    if app.provider_message.contains("ready") {
        Color::Green
    } else if app.provider_message.contains("missing") || app.provider_message.contains("setup") {
        Color::Yellow
    } else {
        Color::DarkGray
    }
}

fn has_active_task(app: &TuiApp) -> bool {
    matches!(
        app.projection.as_ref().map(|projection| projection.status),
        Some(golutra_core::TaskStatus::Running)
            | Some(golutra_core::TaskStatus::WaitingApproval)
            | Some(golutra_core::TaskStatus::Aborting)
            | Some(golutra_core::TaskStatus::Pausing)
            | Some(golutra_core::TaskStatus::Paused)
    )
}

fn composer_style(app: &TuiApp) -> Style {
    if let Some(dialog) = &app.auth_dialog {
        let empty = match dialog.step {
            AuthDialogStep::GroupChoice
            | AuthDialogStep::ThirdPartyChoice
            | AuthDialogStep::AuthMethod
            | AuthDialogStep::Protocol
            | AuthDialogStep::CredentialStore => true,
            AuthDialogStep::BaseUrl => dialog.base_url.is_empty(),
            AuthDialogStep::ApiKey => dialog.api_key.is_empty(),
            AuthDialogStep::EnvKey => dialog.api_key_env.is_empty(),
            AuthDialogStep::Model => dialog.is_custom_model_selected() && dialog.model.is_empty(),
            AuthDialogStep::AdvancedConfig => false,
            AuthDialogStep::Review => false,
        };
        return if empty {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::White)
        };
    }
    if app.input.is_empty() {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::White)
    }
}

fn short_id(value: &str) -> String {
    if value.len() <= 12 {
        value.to_owned()
    } else {
        format!("{}...", &value[..12])
    }
}

fn compact_ack_reason(reason: &Option<String>) -> String {
    match reason.as_deref() {
        Some(value) if value.starts_with("started task ") => "task started".to_owned(),
        Some(value) if value.starts_with("session already has an active") => {
            "session already has an active task".to_owned()
        }
        Some(value) => value.to_owned(),
        None => "prompt accepted".to_owned(),
    }
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

fn session_command(
    session_id: SessionId,
    kind: SessionCommandKind,
    payload: Value,
) -> SessionCommand {
    SessionCommand {
        command_id: CommandId::new(),
        session_id: Some(session_id),
        kind,
        idempotency_key: CommandId::new().to_string(),
        actor: Actor {
            kind: ActorKind::Tui,
            id: TUI_ACTOR_ID.as_str().to_owned(),
        },
        payload,
        timestamp: chrono::Utc::now(),
    }
}

async fn initial_session(
    value: Option<&str>,
    transport: &RuntimeTransport,
) -> miette::Result<(ThreadId, SessionId)> {
    if let Some(value) = value {
        let session_id = Uuid::parse_str(value)
            .map(SessionId)
            .map_err(|error| miette::miette!("invalid session id: {error}"))?;
        let thread_id = transport
            .thread_for_session(session_id)
            .await
            .map_err(|error| miette::miette!("{error}"))?
            .map_or_else(ThreadId::new, |thread| thread.thread_id);
        return Ok((thread_id, session_id));
    }
    Ok((ThreadId::new(), SessionId::new()))
}

fn initial_auth_dialog() -> Option<AuthDialogState> {
    match provider_onboarding_state() {
        Ok(state) if state.configured => None,
        Ok(_) | Err(_) => Some(AuthDialogState::new()),
    }
}

fn parse_task_id(value: Option<&str>) -> miette::Result<Option<TaskId>> {
    value
        .map(|value| {
            Uuid::parse_str(value)
                .map(TaskId)
                .map_err(|error| miette::miette!("invalid task id: {error}"))
        })
        .transpose()
}

fn parse_thread_id(value: &str) -> miette::Result<ThreadId> {
    value
        .parse()
        .map_err(|error: uuid::Error| miette::miette!("invalid thread id: {error}"))
}

fn parse_turn_id(value: &str) -> miette::Result<TurnId> {
    value
        .parse()
        .map_err(|error: uuid::Error| miette::miette!("invalid turn id: {error}"))
}

fn provider_paths_for_tui() -> miette::Result<ProviderConfigPaths> {
    ProviderConfigPaths::global().map_err(|error| miette::miette!("{error}"))
}

fn provider_cwd_for_tui(transport: &RuntimeTransport) -> miette::Result<&std::path::Path> {
    transport
        .cwd()
        .ok_or_else(|| miette::miette!("provider config requires a cwd"))
}

fn resolve_auth_descriptor_path(cwd: &std::path::Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn load_oauth_descriptor_for_tui(
    path: &std::path::Path,
) -> Result<OAuthProviderDescriptor, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|error| format!("failed to read OAuth descriptor: {error}"))?;
    let descriptor: OAuthProviderDescriptor = serde_json::from_str(&content)
        .map_err(|error| format!("OAuth descriptor JSON is invalid: {error}"))?;
    descriptor.validate().map_err(|error| error.to_string())?;
    Ok(descriptor)
}

fn oauth_credential_source(store: AuthCredentialStore) -> Result<CredentialSource, String> {
    match store {
        AuthCredentialStore::Disk => Ok(CredentialSource::Disk),
        AuthCredentialStore::Ephemeral => Ok(CredentialSource::Ephemeral),
        AuthCredentialStore::Environment => {
            Err("OAuth login requires disk or ephemeral storage".to_owned())
        }
    }
}

async fn run_oauth_login_task(
    paths: ProviderConfigPaths,
    cwd: PathBuf,
    descriptor: OAuthProviderDescriptor,
    command: OAuthLoginCommand,
    cancellation: CancellationToken,
    progress: mpsc::UnboundedSender<AuthTaskProgress>,
) -> Result<AuthTaskOutcome, String> {
    descriptor.validate().map_err(|error| error.to_string())?;
    if !descriptor.flows.contains(&command.flow) {
        return Err(format!(
            "OAuth descriptor `{}` does not support {:?}",
            descriptor.provider_id, command.flow
        ));
    }
    let validation_reference = CredentialRef::ephemeral(SecretKind::OAuthTokenSet);
    let mut profile = ProviderProfile::live_profile(
        command.profile.clone(),
        command.protocol,
        command.base_url,
        command.model,
        validation_reference,
    )
    .map_err(|error| error.to_string())?;
    profile.oauth = Some(descriptor.clone());
    profile.generation_config = command.generation_config;
    profile.validate().map_err(|error| error.to_string())?;
    let auth = provider_auth_service(&paths).map_err(|error| error.to_string())?;
    let source = oauth_credential_source(command.credential_store)?;
    let login_result = match command.flow {
        OAuthFlow::BrowserPkce => {
            let login = auth
                .begin_browser_login(descriptor.clone(), source)
                .await
                .map_err(|error| error.to_string())?;
            let authorization_url = login.authorization_url().to_owned();
            let _ = progress.send(AuthTaskProgress {
                title: "OAuth authorization".to_owned(),
                body: vec![
                    "Open this URL in your browser:".to_owned(),
                    authorization_url,
                ],
            });
            if !command.no_open_browser
                && let Err(error) = login.open_browser().await
            {
                let _ = progress.send(AuthTaskProgress {
                    title: "OAuth browser".to_owned(),
                    body: vec![format!(
                        "browser could not be opened automatically: {error}"
                    )],
                });
            }
            tokio::select! {
                () = cancellation.cancelled() => return Err("OAuth authorization cancelled".to_owned()),
                result = login.complete() => result.map_err(|error| error.to_string())?,
            }
        }
        OAuthFlow::DeviceCode => {
            let login = auth
                .begin_device_login(descriptor.clone(), source)
                .await
                .map_err(|error| error.to_string())?;
            let verification_url = login
                .verification_uri_complete()
                .unwrap_or_else(|| login.verification_uri());
            let _ = progress.send(AuthTaskProgress {
                title: "OAuth device authorization".to_owned(),
                body: vec![
                    format!("Open {verification_url}"),
                    format!("Enter code {}", login.user_code()),
                ],
            });
            tokio::select! {
                () = cancellation.cancelled() => return Err("OAuth authorization cancelled".to_owned()),
                result = login.complete() => result.map_err(|error| error.to_string())?,
            }
        }
        OAuthFlow::OpenAiDeviceAuth => {
            let login = auth
                .begin_openai_device_login(descriptor.clone(), source)
                .await
                .map_err(|error| error.to_string())?;
            let _ = progress.send(AuthTaskProgress {
                title: "OAuth device authorization".to_owned(),
                body: vec![
                    format!("Open {}", login.verification_uri()),
                    format!("Enter code {}", login.user_code()),
                ],
            });
            if !command.no_open_browser
                && let Err(error) = login.open_browser().await
            {
                let _ = progress.send(AuthTaskProgress {
                    title: "OAuth browser".to_owned(),
                    body: vec![format!(
                        "browser could not be opened automatically: {error}"
                    )],
                });
            }
            tokio::select! {
                () = cancellation.cancelled() => return Err("OAuth authorization cancelled".to_owned()),
                result = login.complete() => result.map_err(|error| error.to_string())?,
            }
        }
    };
    if cancellation.is_cancelled() {
        let _ = auth
            .logout(&login_result.credential_ref, Some(&descriptor))
            .await;
        return Err("OAuth authorization cancelled".to_owned());
    }
    profile.credential_ref = Some(login_result.credential_ref);
    apply_oauth_provider_install_plan_verified(
        &paths,
        &cwd,
        &ProviderInstallPlan {
            scope: ProviderConfigScope::User,
            profile,
            activate: true,
            pending_secret: None,
        },
    )
    .await
    .map_err(|error| error.to_string())?;
    Ok(AuthTaskOutcome {
        title: "Auth updated".to_owned(),
        body: vec![
            format!("provider profile {} connected", command.profile),
            format!("OAuth provider {}", descriptor.provider_id),
            format!(
                "token refresh {}",
                if login_result.metadata.refreshable {
                    "enabled"
                } else {
                    "requires re-login after expiry"
                }
            ),
        ],
    })
}

fn provider_scope(scope: AuthConfigScope) -> ProviderConfigScope {
    match scope {
        AuthConfigScope::User => ProviderConfigScope::User,
        AuthConfigScope::Workspace => ProviderConfigScope::Workspace,
    }
}

fn auth_login(dialog: &AuthDialogState) -> Result<OpenAiCompatibleLogin, String> {
    let provider = dialog.provider.unwrap_or(CUSTOM_PROVIDER_PRESET);
    let api_key_env = if dialog.api_key_env.trim().is_empty() {
        suggested_api_key_env(dialog)
    } else {
        dialog.api_key_env.trim().to_owned()
    };
    Ok(OpenAiCompatibleLogin {
        profile: provider.profile.to_owned(),
        protocol: dialog.protocol,
        base_url: dialog.base_url.trim().to_owned(),
        model: dialog.model.trim().to_owned(),
        api_key_env,
        api_key: (dialog.credential_store != AuthCredentialStore::Environment)
            .then(|| dialog.api_key.trim().to_owned()),
        credential_store: dialog.credential_store,
        credential_ref: dialog
            .review
            .as_ref()
            .map(|review| review.credential_ref.clone()),
        generation_config: validate_generation_config(dialog)?,
        scope: AuthConfigScope::User,
    })
}

fn suggested_api_key_env(dialog: &AuthDialogState) -> String {
    match dialog.provider.unwrap_or(CUSTOM_PROVIDER_PRESET).source {
        AuthProviderSource::Custom => {
            generate_custom_provider_api_key_env(dialog.protocol, dialog.base_url.trim())
        }
        AuthProviderSource::Official | AuthProviderSource::ThirdParty => {
            "GOLUTRA_PROVIDER_API_KEY".to_owned()
        }
    }
}

fn build_auth_review(dialog: &AuthDialogState) -> Result<AuthReview, String> {
    let provider = dialog.provider.unwrap_or(CUSTOM_PROVIDER_PRESET);
    let login = auth_login(dialog)?;
    let paths = provider_paths_for_tui().map_err(|error| error.to_string())?;
    let scope = provider_scope(login.scope);
    let config_path = paths.user_config.clone();
    let settings = load_provider_settings(&paths).map_err(|error| error.to_string())?;
    let updates_existing_profile = settings
        .profiles
        .iter()
        .any(|profile| profile.name == login.profile);

    let (credential_ref, _) = credential_for_login(&login)?;
    let mut preview_profile = ProviderProfile::live_profile(
        login.profile.clone(),
        login.protocol,
        login.base_url.clone(),
        login.model.clone(),
        credential_ref.clone(),
    )
    .map_err(|error| error.to_string())?;
    preview_profile.generation_config = login.generation_config.clone();
    let preview_plan = ProviderInstallPlan {
        scope,
        profile: preview_profile,
        activate: true,
        pending_secret: None,
    };
    let preview_json =
        serde_json::to_string_pretty(&preview_plan).map_err(|error| error.to_string())?;

    Ok(AuthReview {
        provider_title: provider.title,
        profile: login.profile,
        protocol: login.protocol.id().to_owned(),
        base_url: login.base_url,
        model: login.model,
        credential: match login.credential_store {
            AuthCredentialStore::Environment => format!("env:{}", login.api_key_env),
            AuthCredentialStore::Disk => "disk:$GOLUTRA_HOME/credentials.json".to_owned(),
            AuthCredentialStore::Ephemeral => format!(
                "ephemeral:{}",
                mask_api_key(login.api_key.as_deref().unwrap_or_default())
            ),
        },
        credential_ref,
        advanced: generation_config_summary(login.generation_config.as_ref()),
        scope,
        config_path,
        updates_existing_profile,
        preview_json,
    })
}

fn validate_auth_base_url(value: &str) -> Result<String, String> {
    let trimmed = value.trim().trim_end_matches('/').to_owned();
    if trimmed.is_empty() {
        return Err("Base URL cannot be empty".to_owned());
    }
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        return Err("Base URL must start with http:// or https://".to_owned());
    }
    let Some((_, rest)) = trimmed.split_once("://") else {
        return Err("Base URL must start with http:// or https://".to_owned());
    };
    if rest.split('/').next().unwrap_or_default().trim().is_empty() {
        return Err("Base URL host cannot be empty".to_owned());
    }
    Ok(trimmed)
}

fn normalize_model_id(value: &str) -> String {
    value
        .split(',')
        .map(str::trim)
        .find(|model| !model.is_empty())
        .unwrap_or_default()
        .to_owned()
}

fn mask_api_key(value: &str) -> String {
    let length = value.chars().count();
    if length <= 8 {
        return "***".to_owned();
    }
    let prefix = value.chars().take(4).collect::<String>();
    let suffix = value
        .chars()
        .rev()
        .take(4)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("{prefix}...{suffix}")
}

async fn apply_auth_login(
    transport: &RuntimeTransport,
    login: OpenAiCompatibleLogin,
) -> miette::Result<()> {
    let paths = provider_paths_for_tui()?;
    let cwd = provider_cwd_for_tui(transport)?;
    let scope = provider_scope(login.scope);
    let (credential_ref, pending_secret) =
        credential_for_login(&login).map_err(|error| miette::miette!("{error}"))?;
    let mut profile = ProviderProfile::live_profile(
        login.profile,
        login.protocol,
        login.base_url,
        login.model,
        credential_ref,
    )
    .map_err(|error| miette::miette!("{error}"))?;
    profile.generation_config = login.generation_config;
    apply_provider_install_plan_verified(
        &paths,
        cwd,
        &ProviderInstallPlan {
            scope,
            profile,
            activate: true,
            pending_secret,
        },
    )
    .await
    .map_err(|error| miette::miette!("{error}"))?;

    Ok(())
}

fn credential_for_login(
    login: &OpenAiCompatibleLogin,
) -> Result<(CredentialRef, Option<SecretString>), String> {
    match login
        .api_key
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        Some(api_key) => {
            let reference = match (login.credential_ref.clone(), login.credential_store) {
                (Some(reference), AuthCredentialStore::Disk)
                    if matches!(reference.source, CredentialSource::Disk) =>
                {
                    reference
                }
                (Some(reference), AuthCredentialStore::Ephemeral)
                    if matches!(reference.source, CredentialSource::Ephemeral) =>
                {
                    reference
                }
                (Some(_), _) => {
                    return Err("reviewed credential source no longer matches setup".to_owned());
                }
                (None, AuthCredentialStore::Disk) => CredentialRef::disk(SecretKind::ApiKey),
                (None, AuthCredentialStore::Environment) => {
                    return Err(
                        "an inline API key cannot use environment storage; omit --api-key and provide --api-key-env"
                            .to_owned(),
                    );
                }
                (None, AuthCredentialStore::Ephemeral) => {
                    CredentialRef::ephemeral(SecretKind::ApiKey)
                }
            };
            Ok((reference, Some(SecretString::from(api_key.to_owned()))))
        }
        None => match login.credential_ref.clone() {
            Some(reference) if matches!(reference.source, CredentialSource::Environment { .. }) => {
                Ok((reference, None))
            }
            Some(_) => Err("reviewed credential source no longer matches setup".to_owned()),
            None => CredentialRef::environment(login.api_key_env.clone(), SecretKind::ApiKey)
                .map(|reference| (reference, None))
                .map_err(|error| error.to_string()),
        },
    }
}

fn validate_generation_config(
    dialog: &AuthDialogState,
) -> Result<Option<ProviderGenerationConfig>, String> {
    let context_window_size =
        parse_optional_positive_u64(&dialog.context_window_size, "Context window size")?;
    let max_tokens = parse_optional_positive_u64(&dialog.max_tokens, "Max output tokens")?;
    let config = ProviderGenerationConfig {
        enable_thinking: dialog.enable_thinking,
        reasoning_effort: dialog.reasoning_effort,
        context_window_size,
        max_tokens,
    };
    Ok((!config.is_empty()).then_some(config))
}

fn parse_optional_positive_u64(value: &str, label: &str) -> Result<Option<u64>, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let parsed = trimmed
        .parse::<u64>()
        .map_err(|_| format!("{label} must be a positive integer"))?;
    if parsed == 0 {
        return Err(format!("{label} must be a positive integer"));
    }
    Ok(Some(parsed))
}

fn next_reasoning_effort(
    value: Option<ProviderReasoningEffort>,
) -> Option<ProviderReasoningEffort> {
    match value {
        None => Some(ProviderReasoningEffort::Low),
        Some(ProviderReasoningEffort::Low) => Some(ProviderReasoningEffort::Medium),
        Some(ProviderReasoningEffort::Medium) => Some(ProviderReasoningEffort::High),
        Some(ProviderReasoningEffort::High) => Some(ProviderReasoningEffort::Xhigh),
        Some(ProviderReasoningEffort::Xhigh) => None,
    }
}

fn reasoning_effort_label(value: Option<ProviderReasoningEffort>) -> &'static str {
    match value {
        None => "default",
        Some(ProviderReasoningEffort::Low) => "low",
        Some(ProviderReasoningEffort::Medium) => "medium",
        Some(ProviderReasoningEffort::High) => "high",
        Some(ProviderReasoningEffort::Xhigh) => "xhigh",
    }
}

fn generation_config_summary(config: Option<&ProviderGenerationConfig>) -> String {
    let Some(config) = config else {
        return "default".to_owned();
    };
    let mut parts = Vec::new();
    if config.enable_thinking {
        parts.push("thinking=on".to_owned());
    }
    if let Some(reasoning_effort) = config.reasoning_effort {
        parts.push(format!(
            "effort={}",
            reasoning_effort_label(Some(reasoning_effort))
        ));
    }
    if let Some(context_window_size) = config.context_window_size {
        parts.push(format!("context={context_window_size}"));
    }
    if let Some(max_tokens) = config.max_tokens {
        parts.push(format!("max_tokens={max_tokens}"));
    }
    if parts.is_empty() {
        "default".to_owned()
    } else {
        parts.join(", ")
    }
}

fn apply_auth_mock() -> miette::Result<()> {
    let paths = provider_paths_for_tui()?;
    ProviderInstallPlan {
        scope: ProviderConfigScope::User,
        profile: ProviderProfile::mock(),
        activate: true,
        pending_secret: None,
    }
    .apply(&paths)
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
        "/debug  toggle debug timeline".to_owned(),
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

fn provider_status_message() -> String {
    match provider_onboarding_state() {
        Ok(state) if state.configured => {
            let profile = state
                .active_profile
                .map(|profile| profile.name)
                .unwrap_or_else(|| "default".to_owned());
            format!("ready ({profile})")
        }
        Ok(state) => {
            let missing = if state.missing_fields.is_empty() {
                "provider setup".to_owned()
            } else {
                state.missing_fields.join(", ")
            };
            format!("missing {missing}; use /auth setup")
        }
        Err(error) => format!("provider config error: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use golutra_auth::{CredentialSource, OpenAiDeviceAuthorizationDescriptor};
    use golutra_config::ProviderSettings;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::{Mutex, MutexGuard},
    };

    use super::*;

    static ENV_LOCK: Mutex<()> = Mutex::const_new(());

    async fn env_lock_guard() -> MutexGuard<'static, ()> {
        ENV_LOCK.lock().await
    }

    async fn spawn_probe_server(body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut buffer = [0_u8; 2048];
            let _ = stream.read(&mut buffer).await.expect("read request");
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });
        format!("http://{address}/v1")
    }

    async fn spawn_oauth_probe_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut buffer = [0_u8; 4_096];
                let read = stream.read(&mut buffer).await.expect("read request");
                let request = String::from_utf8_lossy(&buffer[..read]);
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .expect("path");
                let body = match path {
                    "/device" => serde_json::json!({
                        "device_code": "device-secret",
                        "user_code": "GOLUTRA-123",
                        "verification_uri": "https://example.com/device",
                        "expires_in": 600,
                        "interval": 1
                    })
                    .to_string(),
                    "/openai-usercode" => serde_json::json!({
                        "device_auth_id": "openai-device-secret",
                        "user_code": "OPENAI-123",
                        "interval": "1"
                    })
                    .to_string(),
                    "/openai-poll" => serde_json::json!({
                        "authorization_code": "openai-device-code",
                        "code_verifier": "openai-device-verifier"
                    })
                    .to_string(),
                    "/token" => serde_json::json!({
                        "access_token": "oauth-access-token",
                        "refresh_token": "oauth-refresh-token",
                        "token_type": "Bearer",
                        "expires_in": 3600
                    })
                    .to_string(),
                    "/v1/models" => serde_json::json!({
                        "data": [{"id": "oauth-model"}]
                    })
                    .to_string(),
                    other => panic!("unexpected OAuth test path {other}"),
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
            }
        });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn initial_session_without_argument_starts_new_thread_and_session() {
        let transport = RuntimeTransport::in_memory().await.expect("transport");
        let (thread_id, session_id) = initial_session(None, &transport)
            .await
            .expect("initial session");

        assert_ne!(thread_id, transport.default_thread_id());
        assert_ne!(session_id, transport.default_session_id());
    }

    #[tokio::test]
    async fn initial_session_with_argument_keeps_explicit_session() {
        let transport = RuntimeTransport::in_memory().await.expect("transport");
        let explicit_session_id = SessionId::new();
        let (thread_id, session_id) =
            initial_session(Some(&explicit_session_id.to_string()), &transport)
                .await
                .expect("initial session");

        assert_ne!(thread_id, transport.default_thread_id());
        assert_eq!(session_id, explicit_session_id);
    }

    #[tokio::test]
    async fn initial_session_resolves_the_thread_owned_by_an_existing_session() {
        let transport = RuntimeTransport::in_memory().await.expect("transport");
        let session_id = SessionId::new();
        let thread_id = ThreadId::new();
        transport
            .send_command(session_command(
                session_id,
                SessionCommandKind::Prompt,
                json!({
                    "prompt": "existing session",
                    "_thread_id": thread_id.to_string(),
                }),
            ))
            .await
            .expect("prompt");

        let resolved = initial_session(Some(&session_id.to_string()), &transport)
            .await
            .expect("initial session");

        assert_eq!(resolved, (thread_id, session_id));
    }

    #[tokio::test]
    async fn initial_auth_dialog_opens_without_provider_config() {
        let home = tempfile::tempdir().expect("home");
        let _guard = env_lock_guard().await;
        let previous_home = std::env::var("GOLUTRA_HOME").ok();
        unsafe {
            std::env::set_var("GOLUTRA_HOME", home.path());
        }
        assert!(initial_auth_dialog().is_some());
        match previous_home {
            Some(value) => unsafe {
                std::env::set_var("GOLUTRA_HOME", value);
            },
            None => unsafe {
                std::env::remove_var("GOLUTRA_HOME");
            },
        }
    }

    #[tokio::test]
    async fn auth_dialog_mock_choice_persists_global_provider() {
        let dir = tempfile::tempdir().expect("dir");
        let home = tempfile::tempdir().expect("home");
        let _guard = env_lock_guard().await;
        let previous_home = std::env::var("GOLUTRA_HOME").ok();
        unsafe {
            std::env::set_var("GOLUTRA_HOME", home.path());
        }
        let transport = RuntimeTransport::for_cwd(dir.path())
            .await
            .expect("transport");
        let mut app = TuiApp::new(
            ThreadId::new(),
            SessionId::new(),
            None,
            false,
            provider_status_message(),
            Some(AuthDialogState::new()),
        );
        let dialog = app.auth_dialog.as_mut().expect("dialog");
        dialog.selected = 3;

        let result = advance_auth_dialog(&mut app, &transport).await;

        result.expect("advance");
        assert!(app.auth_dialog.is_none());
        assert_eq!(app.provider_message, "ready (mock)");
        assert!(initial_auth_dialog().is_none());
        let settings = ProviderSettings::load(home.path().join("provider.json")).expect("settings");
        assert_eq!(
            settings.active_profile().expect("profile").protocol,
            ProviderProtocol::Mock
        );
        assert!(!dir.path().join(".golutra/provider.json").exists());
        match previous_home {
            Some(value) => unsafe {
                std::env::set_var("GOLUTRA_HOME", value);
            },
            None => unsafe {
                std::env::remove_var("GOLUTRA_HOME");
            },
        }
    }

    #[tokio::test]
    async fn auth_dialog_openai_flow_persists_user_key() {
        let dir = tempfile::tempdir().expect("dir");
        let home = tempfile::tempdir().expect("home");
        let base_url = spawn_probe_server(r#"{"data":[{"id":"qwen-coder"}]}"#).await;
        let _guard = env_lock_guard().await;
        let previous_home = std::env::var("GOLUTRA_HOME").ok();
        unsafe {
            std::env::set_var("GOLUTRA_HOME", home.path());
        }
        let transport = RuntimeTransport::for_cwd(dir.path())
            .await
            .expect("transport");
        let mut app = TuiApp::new(
            ThreadId::new(),
            SessionId::new(),
            None,
            false,
            provider_status_message(),
            Some(AuthDialogState::new()),
        );
        {
            let dialog = app.auth_dialog.as_mut().expect("dialog");
            dialog.provider = Some(OFFICIAL_PROVIDER_PRESET);
            dialog.step = AuthDialogStep::BaseUrl;
            dialog.base_url = base_url;
            dialog.model = "qwen-coder".to_owned();
            dialog.api_key = "test-key".to_owned();
        }

        advance_auth_dialog(&mut app, &transport)
            .await
            .expect("base url");
        advance_auth_dialog(&mut app, &transport)
            .await
            .expect("api key");
        {
            let dialog = app.auth_dialog.as_mut().expect("dialog");
            dialog.selected = dialog.custom_model_index();
        }
        advance_auth_dialog(&mut app, &transport)
            .await
            .expect("model");
        assert_eq!(
            app.auth_dialog.as_ref().map(|dialog| dialog.step),
            Some(AuthDialogStep::AdvancedConfig)
        );
        advance_auth_dialog(&mut app, &transport)
            .await
            .expect("advanced config");
        assert_eq!(
            app.auth_dialog.as_ref().map(|dialog| dialog.step),
            Some(AuthDialogStep::Review)
        );
        let reviewed_credential_id = app
            .auth_dialog
            .as_ref()
            .and_then(|dialog| dialog.review.as_ref())
            .map(|review| review.credential_ref.id.clone())
            .expect("reviewed credential");

        let result = advance_auth_dialog(&mut app, &transport).await;
        result.expect("advance");

        assert!(app.auth_dialog.is_none());
        assert_eq!(app.provider_message, "ready (golutra)");
        let settings = ProviderSettings::load(home.path().join("provider.json")).expect("settings");
        let profile = settings.active_profile().expect("profile");
        assert_eq!(profile.name, "golutra");
        assert_eq!(profile.model_id.as_deref(), Some("qwen-coder"));
        assert!(matches!(
            profile
                .credential_ref
                .as_ref()
                .map(|reference| &reference.source),
            Some(CredentialSource::Ephemeral)
        ));
        assert_eq!(
            profile
                .credential_ref
                .as_ref()
                .map(|reference| reference.id.as_str()),
            Some(reviewed_credential_id.as_str())
        );
        let persisted = std::fs::read_to_string(home.path().join("provider.json"))
            .expect("persisted provider settings");
        assert!(!persisted.contains("test-key"));
        assert!(!dir.path().join(".golutra/provider.json").exists());
        match previous_home {
            Some(value) => unsafe {
                std::env::set_var("GOLUTRA_HOME", value);
            },
            None => unsafe {
                std::env::remove_var("GOLUTRA_HOME");
            },
        }
    }

    #[tokio::test]
    async fn auth_dialog_base_url_requires_http_scheme() {
        let transport = RuntimeTransport::in_memory().await.expect("transport");
        let mut app = TuiApp::new(
            ThreadId::new(),
            SessionId::new(),
            None,
            false,
            provider_status_message(),
            Some(AuthDialogState::new()),
        );
        {
            let dialog = app.auth_dialog.as_mut().expect("dialog");
            dialog.provider = Some(OFFICIAL_PROVIDER_PRESET);
            dialog.step = AuthDialogStep::BaseUrl;
            dialog.base_url = "api.golutra.cn".to_owned();
        }

        advance_auth_dialog(&mut app, &transport)
            .await
            .expect("advance");

        let dialog = app.auth_dialog.as_ref().expect("dialog");
        assert_eq!(dialog.step, AuthDialogStep::BaseUrl);
        assert_eq!(
            dialog.error.as_deref(),
            Some("Base URL must start with http:// or https://")
        );
    }

    #[tokio::test]
    async fn q_key_does_not_exit_tui() {
        let transport = RuntimeTransport::in_memory().await.expect("transport");
        let mut app = TuiApp::new(
            ThreadId::new(),
            SessionId::new(),
            None,
            false,
            "ready (mock)".to_owned(),
            None,
        );

        handle_key(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            &mut app,
            &transport,
        )
        .await
        .expect("handle key");

        assert!(!app.should_quit);
        assert_eq!(app.input, "q");
    }

    #[tokio::test]
    async fn ctrl_c_requires_second_press_to_exit() {
        let transport = RuntimeTransport::in_memory().await.expect("transport");
        let mut app = TuiApp::new(
            ThreadId::new(),
            SessionId::new(),
            None,
            false,
            "ready (mock)".to_owned(),
            None,
        );
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

        handle_key(ctrl_c, &mut app, &transport)
            .await
            .expect("first ctrl-c");
        assert!(!app.should_quit);
        assert_eq!(app.status_message, "press Ctrl+C again to quit");

        handle_key(ctrl_c, &mut app, &transport)
            .await
            .expect("second ctrl-c");
        assert!(app.should_quit);
    }

    #[test]
    fn slash_candidates_render_below_composer_with_selection() {
        let mut app = TuiApp::new(
            ThreadId::new(),
            SessionId::new(),
            None,
            false,
            "ready (mock)".to_owned(),
            None,
        );
        app.input = "/".to_owned();
        app.slash_selected = 2;
        let candidates = app.slash_candidates();
        let lines = slash_candidate_lines(&app, &candidates)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(lines.contains("/new"));
        assert!(lines.contains("/resume"));
        assert!(lines.contains("› /threads"));
        assert_eq!(bottom_pane_height(&app), 8);
    }

    #[test]
    fn transcript_role_markers_follow_codex_symbols() {
        assert_eq!(role_marker(&TranscriptRole::User), "› ");
        assert_eq!(role_marker(&TranscriptRole::Assistant), "• ");
        assert_eq!(role_marker(&TranscriptRole::Status), "• ");
        assert_eq!(role_marker(&TranscriptRole::System), "• ");
    }

    #[tokio::test]
    async fn auth_review_marks_existing_profile_updates() {
        let home = tempfile::tempdir().expect("home");
        let _guard = env_lock_guard().await;
        let previous_home = std::env::var("GOLUTRA_HOME").ok();
        unsafe {
            std::env::set_var("GOLUTRA_HOME", home.path());
        }
        let paths = provider_paths_for_tui().expect("paths");
        ProviderInstallPlan {
            scope: ProviderConfigScope::User,
            profile: ProviderProfile::openai_compatible(
                "golutra",
                "https://api.golutra.cn/v1",
                "gpt-test",
                CredentialRef::ephemeral(SecretKind::ApiKey),
            )
            .expect("profile"),
            activate: true,
            pending_secret: None,
        }
        .apply(&paths)
        .expect("install");
        let mut dialog = AuthDialogState::new();
        dialog.provider = Some(OFFICIAL_PROVIDER_PRESET);
        dialog.base_url = "https://api.golutra.cn/v1".to_owned();
        dialog.model = "qwen-coder".to_owned();
        dialog.api_key = "test-key".to_owned();

        let review = build_auth_review(&dialog).expect("review");
        match previous_home {
            Some(value) => unsafe {
                std::env::set_var("GOLUTRA_HOME", value);
            },
            None => unsafe {
                std::env::remove_var("GOLUTRA_HOME");
            },
        }

        assert!(review.updates_existing_profile);
        assert_eq!(review.credential, "ephemeral:***");
        assert!(!review.preview_json.contains("\"api_key\""));
        assert!(!review.preview_json.contains("test-key"));
    }

    #[tokio::test]
    async fn auth_review_custom_provider_uses_derived_env_key() {
        let home = tempfile::tempdir().expect("home");
        let _guard = env_lock_guard().await;
        let previous_home = std::env::var("GOLUTRA_HOME").ok();
        unsafe {
            std::env::set_var("GOLUTRA_HOME", home.path());
        }
        let mut dialog = AuthDialogState::new();
        dialog.select_provider(CUSTOM_PROVIDER_PRESET);
        dialog.protocol = ProviderProtocol::OpenAiCompatible;
        dialog.base_url = "https://api.example.com/v1/".to_owned();
        dialog.model = "gpt-5.5".to_owned();
        dialog.credential_store = AuthCredentialStore::Environment;
        dialog.api_key_env = suggested_api_key_env(&dialog);

        let review = build_auth_review(&dialog).expect("review");

        match previous_home {
            Some(value) => unsafe {
                std::env::set_var("GOLUTRA_HOME", value);
            },
            None => unsafe {
                std::env::remove_var("GOLUTRA_HOME");
            },
        }
        let expected_env = generate_custom_provider_api_key_env(
            ProviderProtocol::OpenAiCompatible,
            "https://api.example.com/v1/",
        );
        assert!(review.preview_json.contains(&expected_env));
        assert!(!review.preview_json.contains("test-key"));
        assert!(
            !review
                .preview_json
                .contains("\"api_key_env\": \"GOLUTRA_PROVIDER_API_KEY\"")
        );
    }

    #[tokio::test]
    async fn auth_review_includes_generation_config() {
        let home = tempfile::tempdir().expect("home");
        let _guard = env_lock_guard().await;
        let previous_home = std::env::var("GOLUTRA_HOME").ok();
        unsafe {
            std::env::set_var("GOLUTRA_HOME", home.path());
        }
        let mut dialog = AuthDialogState::new();
        dialog.select_provider(CUSTOM_PROVIDER_PRESET);
        dialog.protocol = ProviderProtocol::OpenAiCompatible;
        dialog.base_url = "https://api.example.com/v1".to_owned();
        dialog.model = "gpt-5.5".to_owned();
        dialog.api_key = "test-key".to_owned();
        dialog.enable_thinking = true;
        dialog.reasoning_effort = Some(ProviderReasoningEffort::High);
        dialog.context_window_size = "128000".to_owned();
        dialog.max_tokens = "512".to_owned();

        let review = build_auth_review(&dialog).expect("review");

        match previous_home {
            Some(value) => unsafe {
                std::env::set_var("GOLUTRA_HOME", value);
            },
            None => unsafe {
                std::env::remove_var("GOLUTRA_HOME");
            },
        }
        assert_eq!(
            review.advanced,
            "thinking=on, effort=high, context=128000, max_tokens=512"
        );
        assert!(review.preview_json.contains("\"enable_thinking\": true"));
        assert!(
            review
                .preview_json
                .contains("\"reasoning_effort\": \"high\"")
        );
        assert!(review.preview_json.contains("\"max_tokens\": 512"));
    }

    #[tokio::test]
    async fn slash_auth_login_persists_native_anthropic_protocol_after_probe() {
        let dir = tempfile::tempdir().expect("dir");
        let home = tempfile::tempdir().expect("home");
        let _guard = env_lock_guard().await;
        let previous_home = std::env::var("GOLUTRA_HOME").ok();
        unsafe {
            std::env::set_var("GOLUTRA_HOME", home.path());
        }
        let transport = RuntimeTransport::for_cwd(dir.path())
            .await
            .expect("transport");
        let base_url = spawn_probe_server(
            r#"{"id":"msg-probe","type":"message","role":"assistant","model":"claude-test","content":[{"type":"text","text":"OK"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#,
        )
        .await;
        let login = OpenAiCompatibleLogin {
            profile: "anthropic".to_owned(),
            protocol: ProviderProtocol::Anthropic,
            base_url,
            model: "claude-test".to_owned(),
            api_key_env: "GOLUTRA_PROVIDER_API_KEY".to_owned(),
            api_key: Some("test-key".to_owned()),
            credential_store: AuthCredentialStore::Ephemeral,
            credential_ref: None,
            generation_config: None,
            scope: AuthConfigScope::User,
        };

        apply_auth_login(&transport, login)
            .await
            .expect("native protocol installed");
        let paths = provider_paths_for_tui().expect("paths");
        let settings = ProviderSettings::load(&paths.user_config).expect("settings");

        match previous_home {
            Some(value) => unsafe {
                std::env::set_var("GOLUTRA_HOME", value);
            },
            None => unsafe {
                std::env::remove_var("GOLUTRA_HOME");
            },
        }
        assert_eq!(
            settings.active_profile().expect("profile").protocol,
            ProviderProtocol::Anthropic
        );
    }

    #[tokio::test]
    async fn auth_dialog_keeps_dialog_open_and_rolls_back_when_probe_fails() {
        let dir = tempfile::tempdir().expect("dir");
        let home = tempfile::tempdir().expect("home");
        let _guard = env_lock_guard().await;
        let previous_home = std::env::var("GOLUTRA_HOME").ok();
        unsafe {
            std::env::set_var("GOLUTRA_HOME", home.path());
        }
        let transport = RuntimeTransport::for_cwd(dir.path())
            .await
            .expect("transport");
        let mut app = TuiApp::new(
            ThreadId::new(),
            SessionId::new(),
            None,
            false,
            provider_status_message(),
            Some(AuthDialogState::new()),
        );
        {
            let dialog = app.auth_dialog.as_mut().expect("dialog");
            dialog.provider = Some(CUSTOM_PROVIDER_PRESET);
            dialog.protocol = ProviderProtocol::OpenAiCompatible;
            dialog.step = AuthDialogStep::Review;
            dialog.base_url = "http://127.0.0.1:9/v1".to_owned();
            dialog.model = "gpt-5.5".to_owned();
            dialog.api_key = "test-key".to_owned();
        }

        handle_auth_dialog_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut app,
            &transport,
        )
        .await
        .expect("enter review");

        let paths = provider_paths_for_tui().expect("paths");
        let settings = ProviderSettings::load(&paths.user_config).expect("settings");

        match previous_home {
            Some(value) => unsafe {
                std::env::set_var("GOLUTRA_HOME", value);
            },
            None => unsafe {
                std::env::remove_var("GOLUTRA_HOME");
            },
        }

        let dialog = app.auth_dialog.as_ref().expect("dialog still open");
        assert_eq!(dialog.step, AuthDialogStep::Review);
        assert!(
            dialog
                .error
                .as_deref()
                .is_some_and(|error| error.contains("provider probe failed"))
        );
        assert_eq!(app.status_message, "provider setup failed");
        assert!(settings.profiles.is_empty());
    }

    #[tokio::test]
    async fn slash_auth_login_failure_reports_error_without_persisting_profile() {
        let dir = tempfile::tempdir().expect("dir");
        let home = tempfile::tempdir().expect("home");
        let _guard = env_lock_guard().await;
        let previous_home = std::env::var("GOLUTRA_HOME").ok();
        unsafe {
            std::env::set_var("GOLUTRA_HOME", home.path());
        }
        let transport = RuntimeTransport::for_cwd(dir.path())
            .await
            .expect("transport");
        let mut app = TuiApp::new(
            ThreadId::new(),
            SessionId::new(),
            None,
            false,
            provider_status_message(),
            None,
        );

        app.execute_auth_command(
            &transport,
            SlashAuthCommand::Login(Box::new(OpenAiCompatibleLogin {
                profile: "custom".to_owned(),
                protocol: ProviderProtocol::OpenAiCompatible,
                base_url: "http://127.0.0.1:9/v1".to_owned(),
                model: "gpt-5.5".to_owned(),
                api_key_env: "GOLUTRA_CUSTOM_PROVIDER_API_KEY_TEST".to_owned(),
                api_key: Some("test-key".to_owned()),
                credential_store: AuthCredentialStore::Ephemeral,
                credential_ref: None,
                generation_config: None,
                scope: AuthConfigScope::User,
            })),
        )
        .await
        .expect("login command");

        let paths = provider_paths_for_tui().expect("paths");
        let settings = ProviderSettings::load(&paths.user_config).expect("settings");

        match previous_home {
            Some(value) => unsafe {
                std::env::set_var("GOLUTRA_HOME", value);
            },
            None => unsafe {
                std::env::remove_var("GOLUTRA_HOME");
            },
        }

        assert_eq!(app.status_message, "provider setup failed");
        assert!(settings.profiles.is_empty());
        assert!(app.command_messages.iter().any(|item| {
            item.title == "Auth failed"
                && item
                    .body
                    .iter()
                    .any(|line| line.contains("provider probe failed"))
        }));
    }

    #[test]
    fn auth_dialog_exposes_qwen_style_provider_groups() {
        let dialog = AuthDialogState::new();
        let group_lines = auth_group_lines(&dialog)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(group_lines.contains("Connect a Provider"));
        assert!(group_lines.contains("Golutra API"));
        assert!(group_lines.contains("Third-party Providers"));
        assert!(group_lines.contains("Custom Provider"));
    }

    #[test]
    fn auth_dialog_exposes_third_party_provider_choices() {
        let mut dialog = AuthDialogState::new();
        dialog.step = AuthDialogStep::ThirdPartyChoice;
        let lines = auth_third_party_lines(&dialog)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(lines.contains("OpenAI"));
        assert!(lines.contains("OpenRouter"));
        assert!(lines.contains("DeepSeek"));
        assert!(lines.contains("Qwen / DashScope compatible"));
    }

    #[test]
    fn auth_dialog_exposes_provider_specific_oauth_methods() {
        let preset = |profile: &str| {
            THIRD_PARTY_PROVIDER_PRESETS
                .iter()
                .find(|preset| preset.profile == profile)
                .copied()
                .expect("provider preset")
        };

        let mut openai = AuthDialogState::new();
        openai.select_provider(preset("openai"));
        assert_eq!(openai.step, AuthDialogStep::AuthMethod);
        assert_eq!(openai.auth_method_count(), 3);
        assert_eq!(
            openai.selected_oauth_method().map(|method| method.protocol),
            Some(ProviderProtocol::OpenAiResponses)
        );
        let openai_lines = auth_method_lines(&openai)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(openai_lines.contains("ChatGPT Pro/Plus (browser)"));
        assert!(openai_lines.contains("ChatGPT Pro/Plus (headless)"));
        assert!(openai_lines.contains("API key"));

        let mut xai = AuthDialogState::new();
        xai.select_provider(preset("xai"));
        assert_eq!(xai.oauth_methods().len(), 2);
        assert_eq!(xai.auth_method_count(), 3);
        let xai_lines = auth_method_lines(&xai)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(xai_lines.contains("xAI Grok OAuth (browser)"));
        assert!(xai_lines.contains("xAI Grok OAuth (headless/device)"));

        let mut copilot = AuthDialogState::new();
        copilot.select_provider(preset("github-copilot"));
        assert_eq!(copilot.auth_method_count(), 1);
        let copilot_lines = auth_method_lines(&copilot)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(copilot_lines.contains("Login with GitHub Copilot"));
        assert!(!copilot_lines.contains("API key"));
    }

    #[test]
    fn auth_dialog_custom_provider_exposes_protocol_step() {
        let mut dialog = AuthDialogState::new();
        dialog.select_provider(CUSTOM_PROVIDER_PRESET);
        assert_eq!(dialog.step, AuthDialogStep::Protocol);

        let lines = auth_protocol_lines(&dialog)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(lines.contains("Custom Provider · Step 1/7 · Protocol"));
        assert!(lines.contains("OpenAI-compatible"));
        assert!(lines.contains("Anthropic-compatible"));
        assert!(lines.contains("Gemini-compatible"));
    }

    #[tokio::test]
    async fn auth_dialog_custom_provider_does_not_prefill_base_url_from_protocol() {
        let transport = RuntimeTransport::in_memory().await.expect("transport");
        let mut app = TuiApp::new(
            ThreadId::new(),
            SessionId::new(),
            None,
            false,
            provider_status_message(),
            Some(AuthDialogState::new()),
        );
        {
            let dialog = app.auth_dialog.as_mut().expect("dialog");
            dialog.select_provider(CUSTOM_PROVIDER_PRESET);
            dialog.selected = 0;
        }

        advance_auth_dialog(&mut app, &transport)
            .await
            .expect("advance");

        let dialog = app.auth_dialog.as_ref().expect("dialog");
        assert_eq!(dialog.step, AuthDialogStep::BaseUrl);
        assert!(dialog.base_url.is_empty());
    }

    #[tokio::test]
    async fn auth_dialog_advances_for_native_custom_protocols() {
        let transport = RuntimeTransport::in_memory().await.expect("transport");
        let mut app = TuiApp::new(
            ThreadId::new(),
            SessionId::new(),
            None,
            false,
            provider_status_message(),
            Some(AuthDialogState::new()),
        );
        {
            let dialog = app.auth_dialog.as_mut().expect("dialog");
            dialog.select_provider(CUSTOM_PROVIDER_PRESET);
            dialog.protocol = ProviderProtocol::Anthropic;
            dialog.step = AuthDialogStep::Model;
            dialog.model = "claude-sonnet-4".to_owned();
            dialog.base_url = "https://api.anthropic.com/v1".to_owned();
            dialog.api_key = "test-key".to_owned();
        }

        advance_auth_dialog(&mut app, &transport)
            .await
            .expect("advance");

        let dialog = app.auth_dialog.as_ref().expect("dialog");
        assert_eq!(dialog.step, AuthDialogStep::AdvancedConfig);
        assert!(dialog.error.is_none());
    }

    #[test]
    fn auth_dialog_exposes_recommended_models_and_custom_input() {
        let mut dialog = AuthDialogState::new();
        dialog.select_provider(OFFICIAL_PROVIDER_PRESET);
        dialog.step = AuthDialogStep::Model;
        let lines = auth_model_lines(&dialog)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(lines.contains("gpt-test"));
        assert!(lines.contains("Custom model"));
    }

    #[tokio::test]
    async fn auth_model_input_accepts_numeric_custom_model_ids() {
        let transport = RuntimeTransport::in_memory().await.expect("transport");
        let mut app = TuiApp::new(
            ThreadId::new(),
            SessionId::new(),
            None,
            false,
            provider_status_message(),
            Some(AuthDialogState::new()),
        );
        {
            let dialog = app.auth_dialog.as_mut().expect("dialog");
            dialog.select_provider(OFFICIAL_PROVIDER_PRESET);
            dialog.step = AuthDialogStep::Model;
            dialog.api_key = "test-key".to_owned();
        }

        for character in "gpt-5.5".chars() {
            handle_auth_dialog_key(
                KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
                &mut app,
                &transport,
            )
            .await
            .expect("type model character");
        }

        let dialog = app.auth_dialog.as_ref().expect("dialog");
        assert_eq!(dialog.step, AuthDialogStep::Model);
        assert!(dialog.is_custom_model_selected());
        assert_eq!(dialog.model, "gpt-5.5");
    }

    #[tokio::test]
    async fn auth_text_inputs_do_not_swallow_vim_key_characters() {
        let transport = RuntimeTransport::in_memory().await.expect("transport");
        let mut app = TuiApp::new(
            ThreadId::new(),
            SessionId::new(),
            None,
            false,
            provider_status_message(),
            Some(AuthDialogState::new()),
        );
        {
            let dialog = app.auth_dialog.as_mut().expect("dialog");
            dialog.select_provider(OFFICIAL_PROVIDER_PRESET);
            dialog.step = AuthDialogStep::ApiKey;
        }

        for character in "sk-key".chars() {
            handle_auth_dialog_key(
                KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
                &mut app,
                &transport,
            )
            .await
            .expect("type api key character");
        }
        {
            let dialog = app.auth_dialog.as_mut().expect("dialog");
            assert_eq!(dialog.api_key, "sk-key");
            dialog.step = AuthDialogStep::Model;
        }
        for character in "jkl-model".chars() {
            handle_auth_dialog_key(
                KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
                &mut app,
                &transport,
            )
            .await
            .expect("type model character");
        }

        let dialog = app.auth_dialog.as_ref().expect("dialog");
        assert_eq!(dialog.model, "jkl-model");
    }

    #[test]
    fn new_idle_session_has_empty_transcript() {
        let mut app = TuiApp::new(
            ThreadId::new(),
            SessionId::new(),
            None,
            false,
            "ready (mock)".to_owned(),
            None,
        );
        app.projection = Some(UserProjection {
            session_id: app.session_id,
            task_id: None,
            status: golutra_core::TaskStatus::Idle,
            visible_steps: Vec::new(),
            pending_approval: None,
            final_message: None,
            residual_risks: Vec::new(),
        });

        assert!(transcript_items(&app).is_empty());
        assert_eq!(bottom_pane_height(&app), 3);
        assert!(provider_footer_line(&app).is_none());
    }

    #[test]
    fn normal_transcript_keeps_only_user_visible_runtime_milestones() {
        let mut app = TuiApp::new(
            ThreadId::new(),
            SessionId::new(),
            None,
            false,
            "ready (mock)".to_owned(),
            None,
        );
        app.projection = Some(UserProjection {
            session_id: app.session_id,
            task_id: None,
            status: golutra_core::TaskStatus::Completed,
            visible_steps: vec![
                VisibleStep {
                    label: "ProviderStarted".to_owned(),
                    status: "Running".to_owned(),
                    summary: "provider request started".to_owned(),
                },
                VisibleStep {
                    label: "ToolCompleted".to_owned(),
                    status: "Running".to_owned(),
                    summary: "file written".to_owned(),
                },
                VisibleStep {
                    label: "TaskCompleted".to_owned(),
                    status: "Completed".to_owned(),
                    summary: "runtime task finished".to_owned(),
                },
            ],
            pending_approval: None,
            final_message: None,
            residual_risks: Vec::new(),
        });

        let items = transcript_items(&app);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Tool Completed");
    }

    #[test]
    fn failed_loop_decision_is_visible_in_transcript() {
        let mut app = TuiApp::new(
            ThreadId::new(),
            SessionId::new(),
            None,
            false,
            "ready (mock)".to_owned(),
            None,
        );
        app.projection = Some(UserProjection {
            session_id: app.session_id,
            task_id: Some(TaskId::new()),
            status: golutra_core::TaskStatus::Failed,
            visible_steps: vec![VisibleStep {
                label: "LoopDecided".to_owned(),
                status: "Running".to_owned(),
                summary: "runtime task execution failed: provider failed: model not found"
                    .to_owned(),
            }],
            pending_approval: None,
            final_message: None,
            residual_risks: Vec::new(),
        });

        let items = transcript_items(&app);

        assert_eq!(items[0].title, "Loop Decided");
        assert!(items[0].body[0].contains("model not found"));
    }

    #[test]
    fn approval_transcript_shows_tool_resource_and_reason() {
        let event = transcript_event(
            1,
            SessionId::new(),
            TaskId::new(),
            RuntimeEventType::ApprovalRequested,
            json!({
                "summary": "approval required for shell",
                "request": {
                    "tool_name": "shell",
                    "resource": "cargo test --workspace",
                    "reason": "process command requires explicit user approval"
                }
            }),
        );

        let items = event_transcript_items(&[event]);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Approval required");
        assert_eq!(items[0].body[0], "shell: cargo test --workspace");
        assert!(items[0].body[1].contains("explicit user approval"));
    }

    fn transcript_event(
        sequence_no: u64,
        session_id: SessionId,
        task_id: TaskId,
        event_type: RuntimeEventType,
        payload: Value,
    ) -> Value {
        serde_json::to_value(RuntimeEvent {
            id: golutra_core::EventId::new(),
            sequence_no,
            session_id,
            turn_id: None,
            task_id: Some(task_id),
            parent_event_id: None,
            event_type,
            timestamp: chrono::Utc::now(),
            source: golutra_protocol::RuntimeEventSource::Runtime,
            payload,
            payload_ref: None,
            durable: true,
        })
        .expect("event serializes")
    }

    #[test]
    fn resumed_completed_history_renders_user_prompt_and_terminal_steps() {
        let session_id = SessionId::new();
        let task_id = TaskId::new();
        let mut app = TuiApp::new(
            ThreadId::new(),
            session_id,
            None,
            false,
            "ready (mock)".to_owned(),
            None,
        );
        app.events = vec![
            transcript_event(
                1,
                session_id,
                task_id,
                RuntimeEventType::TaskCreated,
                json!({
                    "payload": {
                        "prompt": "write file chain.txt with content ok"
                    },
                    "summary": "runtime lane started task",
                }),
            ),
            transcript_event(
                2,
                session_id,
                task_id,
                RuntimeEventType::ToolCompleted,
                json!({"summary": "file written"}),
            ),
            transcript_event(
                3,
                session_id,
                task_id,
                RuntimeEventType::TaskCompleted,
                json!({
                    "summary": "runtime task finished with Completed",
                    "status": "completed",
                }),
            ),
            transcript_event(
                4,
                session_id,
                task_id,
                RuntimeEventType::AssistantMessage,
                json!({
                    "summary": "Completed: file written",
                    "content": "Completed: file written",
                }),
            ),
        ];
        app.projection = Some(UserProjection {
            session_id,
            task_id: Some(task_id),
            status: golutra_core::TaskStatus::Completed,
            visible_steps: vec![
                VisibleStep {
                    label: "ToolCompleted".to_owned(),
                    status: "Running".to_owned(),
                    summary: "file written".to_owned(),
                },
                VisibleStep {
                    label: "TaskCompleted".to_owned(),
                    status: "Completed".to_owned(),
                    summary: "runtime task finished with Completed".to_owned(),
                },
            ],
            pending_approval: None,
            final_message: Some("Completed: file written".to_owned()),
            residual_risks: Vec::new(),
        });

        let items = transcript_items(&app);
        let titles = items
            .iter()
            .map(|item| item.title.as_str())
            .collect::<Vec<_>>();
        let body = items
            .iter()
            .flat_map(|item| item.body.iter())
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");

        assert_eq!(titles, vec!["You", "Tool Completed", "Golutra"]);
        assert!(body.contains("write file chain.txt with content ok"));
        assert!(body.contains("file written"));
        assert!(!body.contains("runtime task finished with Completed"));
        assert!(body.contains("Completed: file written"));
    }

    #[test]
    fn transcript_groups_multiple_turns_from_events_top_to_bottom() {
        let session_id = SessionId::new();
        let first_task = TaskId::new();
        let second_task = TaskId::new();
        let mut app = TuiApp::new(
            ThreadId::new(),
            session_id,
            None,
            false,
            "ready (mock)".to_owned(),
            None,
        );
        app.events = vec![
            transcript_event(
                1,
                session_id,
                first_task,
                RuntimeEventType::TaskCreated,
                json!({"payload": {"prompt": "hi"}}),
            ),
            transcript_event(
                2,
                session_id,
                first_task,
                RuntimeEventType::AssistantMessage,
                json!({"content": "Hello"}),
            ),
            transcript_event(
                3,
                session_id,
                first_task,
                RuntimeEventType::TaskCompleted,
                json!({
                    "summary": "runtime task finished with Completed",
                    "status": "completed"
                }),
            ),
            transcript_event(
                4,
                session_id,
                second_task,
                RuntimeEventType::TaskCreated,
                json!({"payload": {"prompt": "what next"}}),
            ),
            transcript_event(
                5,
                session_id,
                second_task,
                RuntimeEventType::AssistantMessage,
                json!({"content": "Tell me what to work on."}),
            ),
            transcript_event(
                6,
                session_id,
                second_task,
                RuntimeEventType::TaskCompleted,
                json!({
                    "summary": "runtime task finished with Completed",
                    "status": "completed"
                }),
            ),
        ];
        app.projection = Some(UserProjection {
            session_id,
            task_id: Some(second_task),
            status: golutra_core::TaskStatus::Completed,
            visible_steps: Vec::new(),
            pending_approval: None,
            final_message: Some("stale projection final should not duplicate".to_owned()),
            residual_risks: Vec::new(),
        });

        let items = transcript_items(&app);
        let titles = items
            .iter()
            .map(|item| item.title.as_str())
            .collect::<Vec<_>>();
        let bodies = items
            .iter()
            .map(|item| item.body.join("\n"))
            .collect::<Vec<_>>();

        assert_eq!(titles, vec!["You", "Golutra", "You", "Golutra"]);
        assert_eq!(bodies[0], "hi");
        assert_eq!(bodies[1], "Hello");
        assert_eq!(bodies[2], "what next");
        assert_eq!(bodies[3], "Tell me what to work on.");
        assert!(
            bodies
                .iter()
                .all(|body| !body.contains("stale projection final"))
        );
    }

    #[test]
    fn transcript_visible_window_pages_from_bottom_and_round_trips() {
        assert_eq!(transcript_visible_window(50, 10, 0), 40..50);
        assert_eq!(transcript_visible_window(50, 10, 10), 30..40);
        assert_eq!(transcript_visible_window(50, 10, 1_000), 0..10);

        let mut app = TuiApp::new(
            ThreadId::new(),
            SessionId::new(),
            None,
            false,
            "ready (mock)".to_owned(),
            None,
        );
        app.transcript_row_count = 50;

        app.scroll_transcript(TranscriptScrollAction::PageUp, 10);
        assert_eq!(app.transcript_scroll_offset, 10);
        app.scroll_transcript(TranscriptScrollAction::PageDown, 10);
        assert_eq!(app.transcript_scroll_offset, 0);
        app.scroll_transcript(TranscriptScrollAction::Top, 10);
        assert_eq!(app.transcript_scroll_offset, 40);
        app.scroll_transcript(TranscriptScrollAction::Bottom, 10);
        assert_eq!(app.transcript_scroll_offset, 0);
    }

    #[tokio::test]
    async fn resume_thread_clears_previous_visible_transcript_state() {
        let transport = RuntimeTransport::in_memory().await.expect("transport");
        let target_thread_id = ThreadId::new();
        let target_session_id = SessionId::new();
        transport
            .send_command(session_command(
                target_session_id,
                SessionCommandKind::Prompt,
                json!({
                    "prompt": "resume target",
                    "_thread_id": target_thread_id.to_string(),
                }),
            ))
            .await
            .expect("create resumable thread");
        let mut completed = false;
        for _ in 0..100 {
            let projection = transport
                .query(RuntimeQuery {
                    query_id: QueryId::new(),
                    session_id: target_session_id,
                    task_id: None,
                    kind: RuntimeQueryKind::UserProjection,
                    requester: ActorKind::Tui,
                    cursor: None,
                    timestamp: chrono::Utc::now(),
                })
                .await
                .expect("projection");
            if golutra_client::projection_status(&projection)
                == Some(golutra_core::TaskStatus::Completed)
            {
                completed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(completed, "target thread task should complete");
        let mut app = TuiApp::new(
            ThreadId::new(),
            SessionId::new(),
            None,
            false,
            "ready (mock)".to_owned(),
            None,
        );
        app.command_messages.push(TranscriptItem {
            role: TranscriptRole::System,
            title: "Old".to_owned(),
            body: vec!["old session only".to_owned()],
        });
        app.events.push(json!({"old": true}));
        app.input = "/resume".to_owned();
        app.slash_selected = 2;
        app.transcript_scroll_offset = 12;
        app.transcript_row_count = 30;

        app.resume_thread(&transport, target_thread_id)
            .await
            .expect("resume");

        assert!(app.command_messages.is_empty());
        assert!(app.events.is_empty());
        assert!(app.input.is_empty());
        assert_eq!(app.slash_selected, 0);
        assert_eq!(app.transcript_scroll_offset, 0);
    }

    #[test]
    fn start_new_session_resets_visible_tui_state() {
        let original_thread_id = ThreadId::new();
        let original_session_id = SessionId::new();
        let mut app = TuiApp::new(
            original_thread_id,
            original_session_id,
            Some(TaskId::new()),
            true,
            "ready (mock)".to_owned(),
            None,
        );
        app.projection = Some(UserProjection {
            session_id: original_session_id,
            task_id: Some(TaskId::new()),
            status: golutra_core::TaskStatus::Completed,
            visible_steps: Vec::new(),
            pending_approval: None,
            final_message: Some("old answer".to_owned()),
            residual_risks: Vec::new(),
        });
        app.command_messages.push(TranscriptItem {
            role: TranscriptRole::System,
            title: "Old".to_owned(),
            body: vec!["old session only".to_owned()],
        });
        app.events.push(json!({"old": true}));
        app.input = "/new".to_owned();
        app.slash_selected = 2;
        app.cursor = Some(9);
        app.resume_picker = Some(ResumePickerState {
            items: Vec::new(),
            selected: 0,
        });
        app.transcript_scroll_offset = 7;
        app.transcript_row_count = 20;

        app.start_new_session();

        assert_ne!(app.thread_id, original_thread_id);
        assert_ne!(app.session_id, original_session_id);
        assert!(app.task_id.is_none());
        assert!(app.projection.is_none());
        assert!(app.command_messages.is_empty());
        assert!(app.events.is_empty());
        assert!(app.input.is_empty());
        assert_eq!(app.slash_selected, 0);
        assert!(app.cursor.is_none());
        assert!(app.resume_picker.is_none());
        assert!(!app.debug_mode);
        assert_eq!(app.transcript_scroll_offset, 0);
        assert_eq!(app.status_message, "new session");
    }

    #[tokio::test]
    async fn active_task_blocks_session_switching_commands() {
        let transport = RuntimeTransport::in_memory().await.expect("transport");
        let thread_id = ThreadId::new();
        let session_id = SessionId::new();
        let mut app = TuiApp::new(
            thread_id,
            session_id,
            Some(TaskId::new()),
            false,
            "ready (mock)".to_owned(),
            None,
        );
        app.projection = Some(UserProjection {
            session_id,
            task_id: app.task_id,
            status: golutra_core::TaskStatus::Running,
            visible_steps: Vec::new(),
            pending_approval: None,
            final_message: None,
            residual_risks: Vec::new(),
        });

        app.execute_slash_command(&transport, SlashCommand::New)
            .await
            .expect("command");

        assert_eq!(app.thread_id, thread_id);
        assert_eq!(app.session_id, session_id);
        assert_eq!(
            app.status_message,
            "interrupt the active task before switching sessions"
        );
    }

    #[tokio::test]
    async fn auth_dialog_offers_disk_or_environment_reference() {
        let transport = RuntimeTransport::in_memory().await.expect("transport");
        let mut app = TuiApp::new(
            ThreadId::new(),
            SessionId::new(),
            None,
            false,
            "missing provider".to_owned(),
            Some(AuthDialogState::new()),
        );
        {
            let dialog = app.auth_dialog.as_mut().expect("dialog");
            dialog.select_provider(OFFICIAL_PROVIDER_PRESET);
            dialog.credential_store = AuthCredentialStore::Disk;
            dialog.step = AuthDialogStep::BaseUrl;
        }

        advance_auth_dialog(&mut app, &transport)
            .await
            .expect("base URL");
        assert_eq!(
            app.auth_dialog.as_ref().map(|dialog| dialog.step),
            Some(AuthDialogStep::CredentialStore)
        );
        {
            let dialog = app.auth_dialog.as_mut().expect("dialog");
            let lines = auth_credential_store_lines(dialog)
                .into_iter()
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(lines.contains("Local disk"));
            assert!(lines.contains("$GOLUTRA_HOME/credentials.json"));
            assert!(lines.contains("Environment variable"));
            dialog.selected = 1;
        }
        advance_auth_dialog(&mut app, &transport)
            .await
            .expect("credential store");
        let dialog = app.auth_dialog.as_ref().expect("dialog");
        assert_eq!(dialog.step, AuthDialogStep::EnvKey);
        assert_eq!(dialog.api_key_env, "GOLUTRA_PROVIDER_API_KEY");

        let review = build_auth_review(dialog).expect("review");
        assert_eq!(review.credential, "env:GOLUTRA_PROVIDER_API_KEY");
        assert!(review.preview_json.contains("environment"));
        assert!(!review.preview_json.contains("oauth-access-token"));
    }

    #[tokio::test]
    async fn oauth_device_task_installs_secret_ref_and_logout_removes_it() {
        let home = tempfile::tempdir().expect("home");
        let cwd = tempfile::tempdir().expect("cwd");
        let endpoint = spawn_oauth_probe_server().await;
        let descriptor = OAuthProviderDescriptor {
            provider_id: "test-oauth".to_owned(),
            client_id: "test-client".to_owned(),
            authorization_endpoint: format!("{endpoint}/authorize"),
            token_endpoint: format!("{endpoint}/token"),
            device_authorization_endpoint: Some(format!("{endpoint}/device")),
            revocation_endpoint: None,
            scopes: vec!["model.invoke".to_owned()],
            audience: None,
            browser_redirect_uri: None,
            authorization_params: std::collections::BTreeMap::new(),
            authorization_nonce: false,
            openai_device_authorization: None,
            flows: vec![OAuthFlow::DeviceCode],
        };
        let _guard = env_lock_guard().await;
        let previous_home = std::env::var("GOLUTRA_HOME").ok();
        unsafe {
            std::env::set_var("GOLUTRA_HOME", home.path());
        }
        let paths = ProviderConfigPaths::global().expect("paths");
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();

        let outcome = run_oauth_login_task(
            paths.clone(),
            cwd.path().to_path_buf(),
            descriptor,
            OAuthLoginCommand {
                descriptor_path: "oauth.json".to_owned(),
                flow: OAuthFlow::DeviceCode,
                profile: "oauth".to_owned(),
                protocol: ProviderProtocol::OpenAiCompatible,
                base_url: format!("{endpoint}/v1"),
                model: "oauth-model".to_owned(),
                credential_store: AuthCredentialStore::Ephemeral,
                no_open_browser: true,
                generation_config: None,
            },
            CancellationToken::new(),
            progress_tx,
        )
        .await
        .expect("OAuth task");

        assert_eq!(outcome.title, "Auth updated");
        let progress = progress_rx.try_recv().expect("device instructions");
        assert!(progress.body.join(" ").contains("GOLUTRA-123"));
        let persisted = std::fs::read_to_string(&paths.user_config).expect("config");
        assert!(persisted.contains("oauth-token-set"));
        assert!(!persisted.contains("oauth-access-token"));
        assert!(!persisted.contains("oauth-refresh-token"));

        logout_provider_profile_verified(&paths, cwd.path(), "oauth")
            .await
            .expect("logout");
        let settings = ProviderSettings::load(&paths.user_config).expect("settings");
        let profile = settings
            .profiles
            .iter()
            .find(|profile| profile.name == "oauth")
            .expect("profile");
        assert!(!profile.enabled);
        assert!(profile.credential_ref.is_none());

        match previous_home {
            Some(value) => unsafe {
                std::env::set_var("GOLUTRA_HOME", value);
            },
            None => unsafe {
                std::env::remove_var("GOLUTRA_HOME");
            },
        }
    }

    #[tokio::test]
    async fn openai_headless_task_shows_code_and_installs_secret_ref() {
        let home = tempfile::tempdir().expect("home");
        let cwd = tempfile::tempdir().expect("cwd");
        let endpoint = spawn_oauth_probe_server().await;
        let descriptor = OAuthProviderDescriptor {
            provider_id: "openai-test".to_owned(),
            client_id: "test-client".to_owned(),
            authorization_endpoint: format!("{endpoint}/authorize"),
            token_endpoint: format!("{endpoint}/token"),
            device_authorization_endpoint: None,
            revocation_endpoint: None,
            scopes: vec!["model.invoke".to_owned()],
            audience: None,
            browser_redirect_uri: None,
            authorization_params: std::collections::BTreeMap::new(),
            authorization_nonce: false,
            openai_device_authorization: Some(OpenAiDeviceAuthorizationDescriptor {
                user_code_endpoint: format!("{endpoint}/openai-usercode"),
                token_poll_endpoint: format!("{endpoint}/openai-poll"),
                verification_uri: format!("{endpoint}/verify"),
                redirect_uri: format!("{endpoint}/callback"),
            }),
            flows: vec![OAuthFlow::OpenAiDeviceAuth],
        };
        let _guard = env_lock_guard().await;
        let previous_home = std::env::var("GOLUTRA_HOME").ok();
        unsafe {
            std::env::set_var("GOLUTRA_HOME", home.path());
        }
        let paths = ProviderConfigPaths::global().expect("paths");
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();

        let outcome = run_oauth_login_task(
            paths.clone(),
            cwd.path().to_path_buf(),
            descriptor,
            OAuthLoginCommand {
                descriptor_path: "builtin:openai-test:headless".to_owned(),
                flow: OAuthFlow::OpenAiDeviceAuth,
                profile: "openai-headless".to_owned(),
                protocol: ProviderProtocol::OpenAiCompatible,
                base_url: format!("{endpoint}/v1"),
                model: "oauth-model".to_owned(),
                credential_store: AuthCredentialStore::Ephemeral,
                no_open_browser: true,
                generation_config: None,
            },
            CancellationToken::new(),
            progress_tx,
        )
        .await
        .expect("OpenAI headless OAuth task");

        assert_eq!(outcome.title, "Auth updated");
        let progress = progress_rx.try_recv().expect("headless instructions");
        assert!(progress.body.join(" ").contains("OPENAI-123"));
        let persisted = std::fs::read_to_string(&paths.user_config).expect("config");
        assert!(persisted.contains("openai-device-auth"));
        assert!(persisted.contains("oauth-token-set"));
        assert!(!persisted.contains("oauth-access-token"));
        assert!(!persisted.contains("oauth-refresh-token"));

        match previous_home {
            Some(value) => unsafe {
                std::env::set_var("GOLUTRA_HOME", value);
            },
            None => unsafe {
                std::env::remove_var("GOLUTRA_HOME");
            },
        }
    }

    #[test]
    fn compact_ack_reason_hides_runtime_ids() {
        assert_eq!(
            compact_ack_reason(&Some(
                "started task 00000000 in session 11111111".to_owned()
            )),
            "task started"
        );
        assert_eq!(compact_ack_reason(&None), "prompt accepted");
    }

    #[test]
    fn resume_picker_offset_keeps_selected_item_visible() {
        assert_eq!(resume_picker_offset(0, 5, 20), 0);
        assert_eq!(resume_picker_offset(4, 5, 20), 0);
        assert_eq!(resume_picker_offset(5, 5, 20), 1);
        assert_eq!(resume_picker_offset(19, 5, 20), 15);
        assert_eq!(resume_picker_offset(3, 5, 4), 0);
    }
}
