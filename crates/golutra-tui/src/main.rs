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
    event_timeline_lines, parse_slash_input,
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
    ) -> Self {
        Self {
            thread_id,
            session_id,
            task_id,
            projection: None,
            events: Vec::new(),
            command_messages: Vec::new(),
            input: String::new(),
            status_message: "attached to workspace runtime".to_owned(),
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
                json!({ "prompt": prompt }),
            ))
            .await
            .map_err(|error| miette::miette!("{error}"))?;
        self.input.clear();
        self.status_message = ack
            .reason
            .unwrap_or_else(|| "prompt accepted by runtime".to_owned());
        self.refresh(transport).await
    }

    async fn execute_slash_command(
        &mut self,
        transport: &InProcessTransport,
        command: SlashCommand,
    ) -> miette::Result<()> {
        match command {
            SlashCommand::Help => {
                self.push_system_message("Slash commands", slash_help_lines());
            }
            SlashCommand::Auth(command) => {
                self.execute_auth_command(transport, command).await?;
            }
            SlashCommand::Resume { thread_id } => {
                let thread_id = parse_optional_thread_id(thread_id.as_deref(), self.thread_id)?;
                let thread = transport
                    .resume_thread(thread_id)
                    .await
                    .map_err(|error| miette::miette!("{error}"))?;
                self.thread_id = thread.thread_id;
                self.session_id = thread.session_id;
                self.task_id = None;
                self.events.clear();
                self.cursor = None;
                self.status_message =
                    format!("resumed thread {}", short_id(&thread.thread_id.to_string()));
                self.push_system_message(
                    "Thread resumed",
                    vec![
                        format!("thread {}", thread.thread_id),
                        format!("session {}", thread.session_id),
                        format!("title {}", thread.title),
                    ],
                );
                self.refresh(transport).await?;
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

    async fn execute_auth_command(
        &mut self,
        transport: &InProcessTransport,
        command: SlashAuthCommand,
    ) -> miette::Result<()> {
        match command {
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
                let paths = provider_paths_for_tui(transport)?;
                ProviderInstallPlan {
                    scope: ProviderConfigScope::Workspace,
                    profile: ProviderProfile::mock(),
                    activate: true,
                }
                .apply(&paths)
                .map_err(|error| miette::miette!("{error}"))?;
                self.provider_message = provider_status_message(transport);
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

#[tokio::main]
async fn main() -> miette::Result<()> {
    let args = Args::parse();
    let task_id = parse_task_id(args.task_id.as_deref())?;
    let transport = match args.workspace.as_deref() {
        Some(workspace) => InProcessTransport::for_workspace(workspace).await,
        None => InProcessTransport::for_current_workspace().await,
    }
    .map_err(|error| miette::miette!("{error}"))?;
    let session_id = parse_session_id(args.session_id.as_deref(), &transport)?;
    let provider_message = provider_status_message(&transport);
    let mut terminal = setup_terminal()?;
    let result = run_app(
        &mut terminal,
        TuiApp::new(
            transport.default_thread_id(),
            session_id,
            task_id,
            args.debug,
            provider_message,
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

fn draw_ui(frame: &mut Frame<'_>, app: &TuiApp) {
    let constraints = if app.debug_mode {
        vec![
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(8),
            Constraint::Length(5),
        ]
    } else {
        vec![
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(5),
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

fn draw_header(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let task_text = app
        .task_id
        .map(|task_id| task_id.to_string())
        .unwrap_or_else(|| "auto".to_owned());
    let status = app
        .projection
        .as_ref()
        .map(|projection| format!("{:?}", projection.status))
        .unwrap_or_else(|| "loading".to_owned());
    let event_count = app.events.len();
    let lines = vec![
        Line::from(vec![
            Span::styled(
                "Golutra",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(status, Style::default().fg(status_color(app))),
            Span::raw("  "),
            Span::styled(
                if app.debug_mode { "debug" } else { "chat" },
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(vec![
            Span::styled("session ", Style::default().fg(Color::DarkGray)),
            Span::raw(short_id(&app.session_id.to_string())),
            Span::styled("  thread ", Style::default().fg(Color::DarkGray)),
            Span::raw(short_id(&app.thread_id.to_string())),
            Span::styled("  task ", Style::default().fg(Color::DarkGray)),
            Span::raw(short_id(&task_text)),
            Span::styled("  events ", Style::default().fg(Color::DarkGray)),
            Span::raw(event_count.to_string()),
        ]),
        Line::from(vec![
            Span::styled("provider ", Style::default().fg(Color::DarkGray)),
            Span::raw(app.provider_message.clone()),
        ]),
    ];
    let paragraph = Paragraph::new(lines).alignment(Alignment::Left);
    frame.render_widget(paragraph, area);
}

fn draw_transcript(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let mut items = transcript_items(app)
        .into_iter()
        .flat_map(transcript_list_items)
        .collect::<Vec<_>>();
    let visible_rows = area.height.saturating_sub(1) as usize;
    if visible_rows > 0 && items.len() > visible_rows {
        items.drain(0..items.len() - visible_rows);
    }
    let list = List::new(items).block(Block::default().borders(Borders::TOP));
    frame.render_widget(list, area);
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
    let help = "Enter send   /help commands   Ctrl+A abort   Tab debug   q/Esc quit";
    let input_line = if app.input.is_empty() {
        "Ask Golutra to change code or inspect the workspace".to_owned()
    } else {
        app.input.clone()
    };
    let paragraph = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Cyan)),
            Span::styled(input_line, composer_style(app)),
        ]),
        Line::from(vec![
            Span::styled(status_chip(app), Style::default().fg(status_color(app))),
            Span::styled("  ", Style::default()),
            Span::styled(&app.status_message, Style::default().fg(Color::DarkGray)),
        ]),
        Line::from(Span::styled(
            &app.provider_message,
            Style::default().fg(provider_color(app)),
        )),
        Line::from(Span::styled(help, Style::default().fg(Color::DarkGray))),
    ])
    .block(Block::default().borders(Borders::TOP))
    .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn transcript_items(app: &TuiApp) -> Vec<TranscriptItem> {
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

    if items.is_empty() {
        items.push(TranscriptItem {
            role: TranscriptRole::System,
            title: "Ready".to_owned(),
            body: vec!["Type a prompt below to start a task in this workspace.".to_owned()],
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

fn composer_style(app: &TuiApp) -> Style {
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

fn parse_session_id(
    value: Option<&str>,
    transport: &InProcessTransport,
) -> miette::Result<SessionId> {
    value
        .map(|value| {
            Uuid::parse_str(value)
                .map(SessionId)
                .map_err(|error| miette::miette!("invalid session id: {error}"))
        })
        .transpose()
        .map(|session_id| session_id.unwrap_or_else(|| transport.default_session_id()))
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

fn parse_optional_thread_id(value: Option<&str>, fallback: ThreadId) -> miette::Result<ThreadId> {
    value
        .map(parse_thread_id)
        .transpose()
        .map(|thread_id| thread_id.unwrap_or(fallback))
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
    let profile = ProviderProfile::openai_compatible(
        login.profile,
        login.base_url,
        login.model,
        login.api_key_env,
    )
    .map_err(|error| miette::miette!("{error}"))?;
    ProviderInstallPlan {
        scope,
        profile,
        activate: true,
    }
    .apply(&paths)
    .map_err(|error| miette::miette!("{error}"))
}

fn slash_help_lines() -> Vec<String> {
    vec![
        "/resume [thread-id]  resume default or specific thread".to_owned(),
        "/threads [limit]  list recent workspace threads".to_owned(),
        "/fork <thread-id>  fork a thread and switch to it".to_owned(),
        "/auth status  show provider onboarding state".to_owned(),
        "/auth protocols  list registered provider protocols".to_owned(),
        "/auth mock  switch this workspace to mock provider".to_owned(),
        "/auth login --base-url <url> --model <model> [--api-key-env <env>] [--scope user|workspace]".to_owned(),
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
            format!(
                "missing {missing}; run `golutra provider login --base-url <url> --model <model>`"
            )
        }
        Err(error) => format!("provider config error: {error}"),
    }
}
