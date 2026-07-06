use std::{
    io::{self, Stdout},
    time::{Duration, Instant},
};

use clap::Parser;
use crossterm::{
    event::{self, Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use golutra_client::{InProcessTransport, RuntimeClient};
use golutra_config::{
    ProviderConfigPaths, ProviderConfigScope, ProviderInstallPlan, ProviderProfile,
    ProviderSettings, provider_onboarding_state,
};
use golutra_core::{Actor, ActorKind, CommandId, QueryId, SessionId, TaskId, ThreadId};
use golutra_llm::provider_protocol_catalog;
use golutra_protocol::{
    EventFilter, RuntimeEvent, RuntimeEventType, RuntimeQuery, RuntimeQueryKind, SessionCommand,
    SessionCommandKind, UserProjection, VisibleStep,
};
use golutra_tui::{
    AuthConfigScope, OpenAiCompatibleLogin, SlashAuthCommand, SlashCommand, SlashInput,
    event_timeline_lines, parse_slash_input, slash_command_suggestions,
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(name = "golutra-tui")]
#[command(about = "Golutra terminal chat UI")]
struct Args {
    #[arg(long)]
    workspace: Option<std::path::PathBuf>,
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
    input: String,
    status_message: String,
    provider_message: String,
    debug_mode: bool,
    cursor: Option<u64>,
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
            input: String::new(),
            status_message: String::new(),
            provider_message,
            debug_mode,
            cursor: None,
            should_quit: false,
        }
    }

    async fn refresh(&mut self, transport: &InProcessTransport) -> miette::Result<()> {
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

        let new_events = transport
            .subscribe(EventFilter {
                session_id: self.session_id,
                task_id: self.task_id,
                after_sequence_no: self.cursor,
            })
            .await
            .map_err(|error| miette::miette!("{error}"))?;
        self.events.extend(new_events);
        self.cursor = event_timeline_lines(&self.events)
            .into_iter()
            .map(|line| line.sequence_no)
            .max();
        Ok(())
    }

    async fn send_prompt(&mut self, transport: &InProcessTransport) -> miette::Result<()> {
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

    async fn send_runtime_prompt(
        &mut self,
        transport: &InProcessTransport,
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
        self.refresh(transport).await
    }

    async fn execute_slash_command(
        &mut self,
        transport: &InProcessTransport,
        command: SlashCommand,
    ) -> miette::Result<()> {
        match command {
            SlashCommand::Auth(SlashAuthCommand::Setup) => {
                self.open_auth_dialog();
            }
            SlashCommand::Help => {
                self.push_system_message("Slash commands", slash_help_lines());
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
                    vec!["no threads in this workspace yet".to_owned()]
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
            SlashCommand::Fork { thread_id } => {
                let thread = transport
                    .fork_thread(parse_thread_id(&thread_id)?)
                    .await
                    .map_err(|error| miette::miette!("{error}"))?;
                self.thread_id = thread.thread_id;
                self.session_id = thread.session_id;
                self.task_id = None;
                self.events.clear();
                self.cursor = None;
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
            SlashCommand::Abort => {
                self.abort(transport).await?;
            }
            SlashCommand::Clear => {
                self.command_messages.clear();
                self.status_message = "local command messages cleared".to_owned();
            }
            SlashCommand::Quit => {
                self.should_quit = true;
            }
        }
        Ok(())
    }

    async fn open_resume_picker(&mut self, transport: &InProcessTransport) -> miette::Result<()> {
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
            self.push_system_message(
                "Resume",
                vec!["no sessions in this workspace yet".to_owned()],
            );
            return Ok(());
        }

        self.input.clear();
        self.resume_picker = Some(ResumePickerState { items, selected: 0 });
        self.status_message = "select a session to resume".to_owned();
        Ok(())
    }

    async fn resume_thread(
        &mut self,
        transport: &InProcessTransport,
        thread_id: ThreadId,
    ) -> miette::Result<()> {
        let thread = transport
            .resume_thread(thread_id)
            .await
            .map_err(|error| miette::miette!("{error}"))?;
        self.thread_id = thread.thread_id;
        self.session_id = thread.session_id;
        self.task_id = None;
        self.events.clear();
        self.cursor = None;
        self.resume_picker = None;
        self.status_message = format!("resumed {}", short_id(&thread.thread_id.to_string()));
        self.push_system_message(
            "Resumed",
            vec![
                thread.title,
                format!("session {}", short_id(&thread.session_id.to_string())),
            ],
        );
        self.refresh(transport).await
    }

    async fn resume_selected_thread(
        &mut self,
        transport: &InProcessTransport,
    ) -> miette::Result<()> {
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
        transport: &InProcessTransport,
        command: SlashAuthCommand,
    ) -> miette::Result<()> {
        match command {
            SlashAuthCommand::Setup => {
                self.open_auth_dialog();
            }
            SlashAuthCommand::Status => {
                self.provider_message = provider_status_message(transport);
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
                apply_auth_mock(transport)?;
                self.provider_message = provider_status_message(transport);
                self.auth_dialog = None;
                self.push_system_message(
                    "Auth updated",
                    vec!["workspace provider switched to mock".to_owned()],
                );
            }
            SlashAuthCommand::Use { profile, scope } => {
                let paths = provider_paths_for_tui(transport)?;
                let path = match provider_scope(scope) {
                    ProviderConfigScope::User => &paths.user_config,
                    ProviderConfigScope::Workspace => &paths.workspace_config,
                };
                let mut settings =
                    ProviderSettings::load(path).map_err(|error| miette::miette!("{error}"))?;
                settings
                    .set_active_profile(profile.clone())
                    .map_err(|error| miette::miette!("{error}"))?;
                settings
                    .save(path)
                    .map_err(|error| miette::miette!("{error}"))?;
                self.provider_message = provider_status_message(transport);
                self.push_system_message(
                    "Auth updated",
                    vec![format!("active provider profile set to {profile}")],
                );
            }
            SlashAuthCommand::Login(login) => {
                apply_auth_login(transport, login)?;
                self.provider_message = provider_status_message(transport);
                self.auth_dialog = None;
                self.push_system_message(
                    "Auth updated",
                    vec![
                        "OpenAI-compatible provider saved".to_owned(),
                        self.provider_message.clone(),
                    ],
                );
            }
        }
        Ok(())
    }

    async fn abort(&mut self, transport: &InProcessTransport) -> miette::Result<()> {
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

    async fn interrupt_or_quit(&mut self, transport: &InProcessTransport) -> miette::Result<()> {
        if has_active_task(self) {
            self.abort(transport).await?;
        }
        self.should_quit = true;
        Ok(())
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

#[derive(Debug, Clone)]
struct AuthDialogState {
    step: AuthDialogStep,
    selected: usize,
    base_url: String,
    model: String,
    api_key: String,
    error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthDialogStep {
    ProviderChoice,
    BaseUrl,
    Model,
    ApiKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthDialogAction {
    OpenAiCompatible,
    Mock,
    Quit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AuthAdvanceAction {
    None,
    SaveMock,
    SaveOpenAiCompatible(OpenAiCompatibleLogin),
    Quit,
}

impl AuthDialogState {
    fn new() -> Self {
        Self {
            step: AuthDialogStep::ProviderChoice,
            selected: 0,
            base_url: String::new(),
            model: String::new(),
            api_key: String::new(),
            error: None,
        }
    }

    fn selected_action(&self) -> AuthDialogAction {
        match self.selected {
            1 => AuthDialogAction::Mock,
            2 => AuthDialogAction::Quit,
            _ => AuthDialogAction::OpenAiCompatible,
        }
    }

    fn move_selection(&mut self, direction: ResumeSelectionDirection) {
        self.selected = match direction {
            ResumeSelectionDirection::Previous => self.selected.saturating_sub(1),
            ResumeSelectionDirection::Next => (self.selected + 1).min(2),
        };
        self.error = None;
    }

    fn current_input_mut(&mut self) -> Option<&mut String> {
        match self.step {
            AuthDialogStep::BaseUrl => Some(&mut self.base_url),
            AuthDialogStep::Model => Some(&mut self.model),
            AuthDialogStep::ApiKey => Some(&mut self.api_key),
            AuthDialogStep::ProviderChoice => None,
        }
    }

    fn go_back(&mut self) {
        self.error = None;
        self.step = match self.step {
            AuthDialogStep::ProviderChoice => AuthDialogStep::ProviderChoice,
            AuthDialogStep::BaseUrl => AuthDialogStep::ProviderChoice,
            AuthDialogStep::Model => AuthDialogStep::BaseUrl,
            AuthDialogStep::ApiKey => AuthDialogStep::Model,
        };
    }
}

#[derive(Debug, Clone, Copy)]
enum ResumeSelectionDirection {
    Previous,
    Next,
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
    let transport = match args.workspace.as_deref() {
        Some(workspace) => InProcessTransport::for_workspace(workspace).await,
        None => InProcessTransport::for_current_workspace().await,
    }
    .map_err(|error| miette::miette!("{error}"))?;
    let (thread_id, session_id) = initial_session(args.session_id.as_deref(), &transport)?;
    let provider_message = provider_status_message(&transport);
    let auth_dialog = initial_auth_dialog(&transport);
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
    transport: InProcessTransport,
) -> miette::Result<()> {
    app.refresh(&transport).await?;
    let tick_rate = Duration::from_millis(250);
    let mut last_tick = Instant::now();

    while !app.should_quit {
        terminal
            .draw(|frame| draw_ui(frame, &app))
            .map_err(|error| miette::miette!("{error}"))?;

        let timeout = tick_rate.saturating_sub(last_tick.elapsed());
        if event::poll(timeout).map_err(|error| miette::miette!("{error}"))? {
            let event = event::read().map_err(|error| miette::miette!("{error}"))?;
            if let CrosstermEvent::Key(key) = event {
                handle_key(key, &mut app, &transport).await?;
            }
        }

        if last_tick.elapsed() >= tick_rate {
            app.refresh(&transport).await?;
            last_tick = Instant::now();
        }
    }

    Ok(())
}

async fn handle_key(
    key: KeyEvent,
    app: &mut TuiApp,
    transport: &InProcessTransport,
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

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.abort(transport).await?;
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => app.input.clear(),
        KeyCode::Tab => {
            app.debug_mode = !app.debug_mode;
            app.status_message = if app.debug_mode {
                "debug timeline visible".to_owned()
            } else {
                "debug timeline hidden".to_owned()
            };
        }
        KeyCode::Enter => {
            app.send_prompt(transport).await?;
        }
        KeyCode::Backspace => {
            app.input.pop();
        }
        KeyCode::Char(character) => app.input.push(character),
        _ => {}
    }
    Ok(())
}

async fn handle_auth_dialog_key(
    key: KeyEvent,
    app: &mut TuiApp,
    transport: &InProcessTransport,
) -> miette::Result<()> {
    match key.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Esc => {
            if let Some(dialog) = &mut app.auth_dialog {
                dialog.go_back();
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if let Some(dialog) = &mut app.auth_dialog
                && dialog.step == AuthDialogStep::ProviderChoice
            {
                dialog.move_selection(ResumeSelectionDirection::Previous);
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Some(dialog) = &mut app.auth_dialog
                && dialog.step == AuthDialogStep::ProviderChoice
            {
                dialog.move_selection(ResumeSelectionDirection::Next);
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
            advance_auth_dialog(app, transport)?;
        }
        KeyCode::Char(character) if character.is_ascii_digit() => {
            if let Some(dialog) = &mut app.auth_dialog
                && dialog.step == AuthDialogStep::ProviderChoice
                && let Some(index) = character
                    .to_digit(10)
                    .and_then(|digit| digit.checked_sub(1))
                && index < 3
            {
                dialog.selected = index as usize;
                advance_auth_dialog(app, transport)?;
            } else if let Some(input) = app
                .auth_dialog
                .as_mut()
                .and_then(AuthDialogState::current_input_mut)
            {
                input.push(character);
            }
        }
        KeyCode::Char(character) => {
            if let Some(input) = app
                .auth_dialog
                .as_mut()
                .and_then(AuthDialogState::current_input_mut)
            {
                input.push(character);
            }
        }
        _ => {}
    }
    Ok(())
}

fn advance_auth_dialog(app: &mut TuiApp, transport: &InProcessTransport) -> miette::Result<()> {
    let action = {
        let Some(dialog) = &mut app.auth_dialog else {
            return Ok(());
        };
        match dialog.step {
            AuthDialogStep::ProviderChoice => match dialog.selected_action() {
                AuthDialogAction::OpenAiCompatible => {
                    dialog.step = AuthDialogStep::BaseUrl;
                    dialog.error = None;
                    AuthAdvanceAction::None
                }
                AuthDialogAction::Mock => AuthAdvanceAction::SaveMock,
                AuthDialogAction::Quit => AuthAdvanceAction::Quit,
            },
            AuthDialogStep::BaseUrl => {
                if dialog.base_url.trim().is_empty() {
                    dialog.error = Some("Base URL cannot be empty".to_owned());
                } else {
                    dialog.step = AuthDialogStep::Model;
                    dialog.error = None;
                }
                AuthAdvanceAction::None
            }
            AuthDialogStep::Model => {
                if dialog.model.trim().is_empty() {
                    dialog.error = Some("Model cannot be empty".to_owned());
                } else {
                    dialog.step = AuthDialogStep::ApiKey;
                    dialog.error = None;
                }
                AuthAdvanceAction::None
            }
            AuthDialogStep::ApiKey => {
                if dialog.api_key.trim().is_empty() {
                    dialog.error = Some("API key cannot be empty".to_owned());
                    AuthAdvanceAction::None
                } else {
                    AuthAdvanceAction::SaveOpenAiCompatible(OpenAiCompatibleLogin {
                        profile: "default".to_owned(),
                        base_url: dialog.base_url.trim().to_owned(),
                        model: dialog.model.trim().to_owned(),
                        api_key_env: "GOLUTRA_PROVIDER_API_KEY".to_owned(),
                        api_key: Some(dialog.api_key.trim().to_owned()),
                        scope: AuthConfigScope::User,
                    })
                }
            }
        }
    };
    match action {
        AuthAdvanceAction::None => {}
        AuthAdvanceAction::SaveMock => {
            apply_auth_mock(transport)?;
            app.provider_message = provider_status_message(transport);
            app.auth_dialog = None;
            app.status_message = "using mock provider".to_owned();
        }
        AuthAdvanceAction::SaveOpenAiCompatible(login) => {
            apply_auth_login(transport, login)?;
            app.provider_message = provider_status_message(transport);
            app.auth_dialog = None;
            app.status_message = "provider connected".to_owned();
        }
        AuthAdvanceAction::Quit => app.should_quit = true,
    }
    Ok(())
}

async fn handle_resume_picker_key(
    key: KeyEvent,
    app: &mut TuiApp,
    transport: &InProcessTransport,
) -> miette::Result<()> {
    match key.code {
        KeyCode::Esc => app.close_resume_picker(),
        KeyCode::Char('q') => app.should_quit = true,
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
    if app.auth_dialog.is_some()
        || app.resume_picker.is_some()
        || provider_footer_line(app).is_some()
    {
        4
    } else {
        3
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

    let mut items = transcript_items(app)
        .into_iter()
        .flat_map(transcript_list_items)
        .collect::<Vec<_>>();
    if items.is_empty() {
        return;
    }
    let visible_rows = area.height.saturating_sub(1) as usize;
    if visible_rows > 0 && items.len() > visible_rows {
        items.drain(0..items.len() - visible_rows);
    }
    let list = List::new(items).block(Block::default().borders(Borders::TOP));
    frame.render_widget(list, area);
}

fn draw_auth_dialog(frame: &mut Frame<'_>, area: Rect, dialog: &AuthDialogState) {
    let lines = match dialog.step {
        AuthDialogStep::ProviderChoice => auth_choice_lines(dialog),
        AuthDialogStep::BaseUrl => auth_input_lines(
            "Connect provider",
            "Base URL",
            "api.golutra.cn or https://api.openai.com/v1",
            &dialog.base_url,
            dialog.error.as_deref(),
            false,
        ),
        AuthDialogStep::Model => auth_input_lines(
            "Connect provider",
            "Model",
            "model id, for example gpt-4.1 or qwen-coder",
            &dialog.model,
            dialog.error.as_deref(),
            false,
        ),
        AuthDialogStep::ApiKey => auth_input_lines(
            "Connect provider",
            "API key",
            "stored in user provider config",
            &dialog.api_key,
            dialog.error.as_deref(),
            true,
        ),
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

fn auth_choice_lines(dialog: &AuthDialogState) -> Vec<Line<'static>> {
    let options = [
        (
            "OpenAI-compatible",
            "Use an OpenAI-compatible endpoint and API key",
        ),
        ("Continue with mock", "Use local deterministic provider"),
        ("Quit", "Leave without changing provider settings"),
    ];
    let mut lines = vec![Line::from(vec![Span::styled(
        "Connect a provider to start this workspace",
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )])];
    lines.push(Line::from(""));
    lines.extend(
        options
            .into_iter()
            .enumerate()
            .map(|(index, (title, detail))| {
                let selected = index == dialog.selected;
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
                        title,
                        Style::default()
                            .fg(if selected { Color::White } else { Color::Gray })
                            .add_modifier(if selected {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ),
                    Span::raw("  "),
                    Span::styled(detail, Style::default().fg(Color::DarkGray)),
                ])
            }),
    );
    if let Some(error) = &dialog.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            error.clone(),
            Style::default().fg(Color::Red),
        )));
    }
    lines
}

fn auth_input_lines(
    title: &'static str,
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
            title,
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
            "Enter continue   Esc back   Ctrl+C quit",
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
        "Provider setup   Enter continue   Esc back   Ctrl+C quit"
    } else if app.resume_picker.is_some() {
        "Enter resume   Up/Down select   Esc cancel   Ctrl+C quit"
    } else {
        "Enter send   /resume sessions   Ctrl+C quit"
    };
    let command_hint = slash_command_suggestions(&app.input);
    let help_line = if command_hint.is_empty() {
        help.to_owned()
    } else {
        format!("Commands: {}", command_hint.join("   "))
    };
    let input_line = if let Some(dialog) = &app.auth_dialog {
        auth_composer_line(dialog)
    } else if app.resume_picker.is_some() {
        "Select a session to resume".to_owned()
    } else if app.input.is_empty() {
        "Ask Golutra to change code or inspect the workspace".to_owned()
    } else {
        app.input.clone()
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Cyan)),
            Span::styled(input_line, composer_style(app)),
        ]),
        footer_status_line(app),
    ];
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

fn auth_composer_line(dialog: &AuthDialogState) -> String {
    match dialog.step {
        AuthDialogStep::ProviderChoice => "Select provider".to_owned(),
        AuthDialogStep::BaseUrl if dialog.base_url.is_empty() => "Base URL".to_owned(),
        AuthDialogStep::BaseUrl => dialog.base_url.clone(),
        AuthDialogStep::Model if dialog.model.is_empty() => "Model".to_owned(),
        AuthDialogStep::Model => dialog.model.clone(),
        AuthDialogStep::ApiKey if dialog.api_key.is_empty() => "API key".to_owned(),
        AuthDialogStep::ApiKey => "*".repeat(dialog.api_key.chars().count()),
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
    items.extend(app.command_messages.clone());
    items.extend(user_prompt_items(&app.events));
    if let Some(projection) = &app.projection {
        items.extend(projection_items(projection));
    } else {
        items.push(TranscriptItem {
            role: TranscriptRole::System,
            title: "Connecting".to_owned(),
            body: vec!["loading workspace runtime state".to_owned()],
        });
    }

    items
}

fn user_prompt_items(events: &[Value]) -> Vec<TranscriptItem> {
    events
        .iter()
        .filter_map(|value| serde_json::from_value::<RuntimeEvent>(value.clone()).ok())
        .filter(|event| event.event_type == RuntimeEventType::TaskCreated)
        .filter_map(|event| {
            event
                .payload
                .get("payload")
                .and_then(|payload| payload.get("prompt"))
                .and_then(Value::as_str)
                .map(|prompt| TranscriptItem {
                    role: TranscriptRole::User,
                    title: "You".to_owned(),
                    body: vec![prompt.to_owned()],
                })
        })
        .collect()
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

fn significant_step(step: &VisibleStep) -> bool {
    matches!(
        step.label.as_str(),
        "ToolCompleted" | "TaskCompleted" | "CommandRejected" | "BusyPolicyDecided"
    )
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
        TranscriptRole::User => "u ",
        TranscriptRole::Assistant => "g ",
        TranscriptRole::Status => "- ",
        TranscriptRole::System => "* ",
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
        Some(golutra_core::TaskStatus::Failed) | Some(golutra_core::TaskStatus::Blocked) => {
            Color::Red
        }
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
            AuthDialogStep::ProviderChoice => true,
            AuthDialogStep::BaseUrl => dialog.base_url.is_empty(),
            AuthDialogStep::Model => dialog.model.is_empty(),
            AuthDialogStep::ApiKey => dialog.api_key.is_empty(),
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
            id: "golutra-tui".to_owned(),
        },
        payload,
        timestamp: chrono::Utc::now(),
    }
}

fn initial_session(
    value: Option<&str>,
    transport: &InProcessTransport,
) -> miette::Result<(ThreadId, SessionId)> {
    if let Some(value) = value {
        let session_id = Uuid::parse_str(value)
            .map(SessionId)
            .map_err(|error| miette::miette!("invalid session id: {error}"))?;
        return Ok((transport.default_thread_id(), session_id));
    }
    Ok((ThreadId::new(), SessionId::new()))
}

fn initial_auth_dialog(transport: &InProcessTransport) -> Option<AuthDialogState> {
    let workspace_root = transport.workspace_root()?;
    match provider_onboarding_state(workspace_root) {
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

fn provider_paths_for_tui(transport: &InProcessTransport) -> miette::Result<ProviderConfigPaths> {
    let workspace = transport
        .workspace_root()
        .ok_or_else(|| miette::miette!("provider config requires a workspace"))?;
    ProviderConfigPaths::for_workspace(workspace).map_err(|error| miette::miette!("{error}"))
}

fn provider_scope(scope: AuthConfigScope) -> ProviderConfigScope {
    match scope {
        AuthConfigScope::User => ProviderConfigScope::User,
        AuthConfigScope::Workspace => ProviderConfigScope::Workspace,
    }
}

fn apply_auth_login(
    transport: &InProcessTransport,
    login: OpenAiCompatibleLogin,
) -> miette::Result<()> {
    let paths = provider_paths_for_tui(transport)?;
    let scope = provider_scope(login.scope);
    let mut profile = ProviderProfile::openai_compatible(
        login.profile,
        login.base_url,
        login.model,
        login.api_key_env,
    )
    .map_err(|error| miette::miette!("{error}"))?;
    profile.api_key = login.api_key;
    ProviderInstallPlan {
        scope,
        profile,
        activate: true,
    }
    .apply(&paths)
    .map_err(|error| miette::miette!("{error}"))
}

fn apply_auth_mock(transport: &InProcessTransport) -> miette::Result<()> {
    let paths = provider_paths_for_tui(transport)?;
    ProviderInstallPlan {
        scope: ProviderConfigScope::Workspace,
        profile: ProviderProfile::mock(),
        activate: true,
    }
    .apply(&paths)
    .map_err(|error| miette::miette!("{error}"))
}

fn slash_help_lines() -> Vec<String> {
    vec![
        "/resume  open current workspace session list".to_owned(),
        "/resume <thread-id>  resume a specific current-workspace thread".to_owned(),
        "/threads [limit]  list recent workspace threads".to_owned(),
        "/fork <thread-id>  fork a thread and switch to it".to_owned(),
        "/auth status  show provider onboarding state".to_owned(),
        "/auth setup  open provider setup".to_owned(),
        "/auth protocols  list registered provider protocols".to_owned(),
        "/auth mock  switch this workspace to mock provider".to_owned(),
        "/auth login --base-url <url> --model <model> [--api-key <key>|--api-key-env <env>] [--scope user|workspace]".to_owned(),
        "/auth use <profile> [user|workspace]  activate saved provider profile".to_owned(),
        "/status  show current session/task status".to_owned(),
        "/debug  toggle debug timeline".to_owned(),
        "/abort  abort active task".to_owned(),
        "/clear  clear local command messages".to_owned(),
        "/quit  leave TUI".to_owned(),
    ]
}

fn provider_status_message(transport: &InProcessTransport) -> String {
    let Some(workspace_root) = transport.workspace_root() else {
        return "workspace config unavailable".to_owned();
    };
    match provider_onboarding_state(workspace_root) {
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
    use super::*;

    #[tokio::test]
    async fn initial_session_without_argument_starts_new_thread_and_session() {
        let transport = InProcessTransport::in_memory().await.expect("transport");
        let (thread_id, session_id) = initial_session(None, &transport).expect("initial session");

        assert_ne!(thread_id, transport.default_thread_id());
        assert_ne!(session_id, transport.default_session_id());
    }

    #[tokio::test]
    async fn initial_session_with_argument_keeps_explicit_session() {
        let transport = InProcessTransport::in_memory().await.expect("transport");
        let explicit_session_id = SessionId::new();
        let (thread_id, session_id) =
            initial_session(Some(&explicit_session_id.to_string()), &transport)
                .expect("initial session");

        assert_eq!(thread_id, transport.default_thread_id());
        assert_eq!(session_id, explicit_session_id);
    }

    #[tokio::test]
    async fn initial_auth_dialog_opens_without_provider_config() {
        let dir = tempfile::tempdir().expect("dir");
        let transport = InProcessTransport::for_workspace(dir.path())
            .await
            .expect("transport");

        assert!(initial_auth_dialog(&transport).is_some());
    }

    #[tokio::test]
    async fn auth_dialog_mock_choice_persists_workspace_provider() {
        let dir = tempfile::tempdir().expect("dir");
        let transport = InProcessTransport::for_workspace(dir.path())
            .await
            .expect("transport");
        let mut app = TuiApp::new(
            ThreadId::new(),
            SessionId::new(),
            None,
            false,
            provider_status_message(&transport),
            Some(AuthDialogState::new()),
        );
        let dialog = app.auth_dialog.as_mut().expect("dialog");
        dialog.selected = 1;

        advance_auth_dialog(&mut app, &transport).expect("advance");

        assert!(app.auth_dialog.is_none());
        assert_eq!(app.provider_message, "ready (mock)");
        assert!(initial_auth_dialog(&transport).is_none());
    }

    #[tokio::test]
    async fn auth_dialog_openai_flow_persists_user_key() {
        let dir = tempfile::tempdir().expect("dir");
        let home = tempfile::tempdir().expect("home");
        let previous_home = std::env::var("GOLUTRA_HOME").ok();
        unsafe {
            std::env::set_var("GOLUTRA_HOME", home.path());
        }
        let transport = InProcessTransport::for_workspace(dir.path())
            .await
            .expect("transport");
        let mut app = TuiApp::new(
            ThreadId::new(),
            SessionId::new(),
            None,
            false,
            provider_status_message(&transport),
            Some(AuthDialogState::new()),
        );
        {
            let dialog = app.auth_dialog.as_mut().expect("dialog");
            dialog.step = AuthDialogStep::ApiKey;
            dialog.base_url = "api.golutra.cn".to_owned();
            dialog.model = "qwen-coder".to_owned();
            dialog.api_key = "test-key".to_owned();
        }

        let result = advance_auth_dialog(&mut app, &transport);
        match previous_home {
            Some(value) => unsafe {
                std::env::set_var("GOLUTRA_HOME", value);
            },
            None => unsafe {
                std::env::remove_var("GOLUTRA_HOME");
            },
        }
        result.expect("advance");

        assert!(app.auth_dialog.is_none());
        assert_eq!(app.provider_message, "ready (default)");
        let settings = ProviderSettings::load(home.path().join("provider.json")).expect("settings");
        let profile = settings.active_profile().expect("profile");
        assert_eq!(profile.model_id.as_deref(), Some("qwen-coder"));
        assert_eq!(profile.api_key.as_deref(), Some("test-key"));
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

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "Tool Completed");
        assert_eq!(items[1].title, "Task Completed");
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
