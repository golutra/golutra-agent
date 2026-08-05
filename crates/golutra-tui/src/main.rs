use std::{
    collections::HashSet,
    io::{self, Stdout},
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use clap::{Args as ClapArgs, Parser, Subcommand};
use crossterm::{
    cursor::SetCursorStyle,
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event as CrosstermEvent, EventStream,
        KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use golutra_auth::{
    CredentialRef, CredentialSource, OAuthFlow, OAuthProviderDescriptor, SecretKind,
};
use golutra_client::{
    DebugExportCoordinator, DebugExportRequest, RuntimeClient, RuntimeExecutionOptions,
    RuntimeTransport, parse_session_range,
};
use golutra_config::{
    BuiltinOAuthMethod, ProviderConfigPaths, ProviderConfigScope, ProviderInstallPlan,
    ProviderProfile, apply_oauth_provider_install_plan_verified,
    apply_provider_install_plan_verified, generate_custom_provider_api_key_env,
    load_provider_settings, logout_provider_profile_verified, provider_auth_service,
    provider_onboarding_state, provider_protocol_has_runtime_adapter,
    update_provider_settings_verified,
};
use golutra_core::{
    ActorKind, ApprovalRequest, ApprovalScope, EventId, QueryId, SessionId, TaskId, ThreadId,
    TurnId, UserQuestionRequest, UserQuestionResolution,
};
use golutra_llm::{
    ProviderGenerationConfig, ProviderHeaderConfig, ProviderHeaderValue, ProviderProtocol,
    provider_protocol_catalog,
};
use golutra_protocol::{
    CommandAck, DebugProjection, EventPageDirection, EventPageRequest, RuntimeEvent,
    RuntimeEventType, RuntimeQuery, RuntimeQueryKind, SessionCommandKind, UserProjection,
    pending_user_question,
};
use golutra_tui::{
    AuthConfigScope, AuthCredentialStore, OAuthLoginCommand, OpenAiCompatibleLogin,
    PaneScrollState, ReasoningEffortSelection, SlashAuthCommand, SlashCommand,
    SlashCommandCandidate, SlashDebugCommand, SlashInput, TranscriptScrollAction,
    parse_slash_input, slash_command_candidates,
};
use ratatui::{Terminal, TerminalOptions, Viewport, backend::CrosstermBackend};
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

static TUI_ACTOR_ID: LazyLock<String> =
    LazyLock::new(|| format!("golutra-tui-{}-{}", std::process::id(), Uuid::now_v7()));
const TUI_HISTORY_PAGE_SIZE: u32 = 256;

mod activity_view;
mod activity_widget;
mod approval_dialog;
mod auth_flow;
mod auth_state;
mod change_projection;
mod composer_input;
mod composer_support;
mod dashboard;
mod developer_projection;
mod developer_query;
mod developer_view;
mod developer_widget;
mod driver;
mod frame_scheduler;
mod help;
mod history_source;
mod inline_history;
mod interaction;
mod live_status;
mod preferences;
mod provider_status;
mod question_dialog;
mod render;
mod rich_text;
mod runtime_controller;
mod session;
mod settings;
mod terminal_integration;
mod transcript_view;
mod transcript_widget;
pub(crate) use activity_view::*;
pub(crate) use activity_widget::*;
pub(crate) use approval_dialog::*;
pub(crate) use auth_flow::*;
pub(crate) use auth_state::*;
pub(crate) use change_projection::*;
pub(crate) use composer_input::*;
pub(crate) use composer_support::*;
pub(crate) use dashboard::*;
pub(crate) use developer_projection::*;
pub(crate) use developer_query::*;
pub(crate) use developer_view::*;
pub(crate) use developer_widget::*;
pub(crate) use frame_scheduler::*;
pub(crate) use help::*;
pub(crate) use history_source::*;
pub(crate) use inline_history::*;
pub(crate) use interaction::*;
pub(crate) use live_status::*;
pub(crate) use preferences::*;
pub(crate) use provider_status::*;
pub(crate) use question_dialog::*;
pub(crate) use render::*;
pub(crate) use rich_text::*;
pub(crate) use runtime_controller::*;
pub(crate) use session::*;
pub(crate) use settings::*;
pub(crate) use terminal_integration::*;
pub(crate) use transcript_view::*;
pub(crate) use transcript_widget::*;

#[derive(Debug, Parser)]
#[command(name = "golutra-tui")]
#[command(about = "Golutra terminal chat UI")]
struct Args {
    #[arg(long, global = true)]
    cwd: Option<std::path::PathBuf>,
    #[arg(long, global = true, conflicts_with = "connect")]
    daemon: bool,
    #[arg(long, global = true, value_name = "URL", conflicts_with = "daemon")]
    connect: Option<String>,
    #[arg(long, global = true, value_name = "UUID")]
    session_id: Option<String>,
    #[arg(long, global = true, value_name = "UUID")]
    task_id: Option<String>,
    #[arg(long, global = true)]
    debug: bool,
    /// Compatibility flag; inline rendering is now the default.
    #[arg(long, global = true)]
    inline: bool,
    /// Disable workspace, sensitive-path, shell and OS sandbox restrictions
    /// for prompts submitted by this TUI.
    #[arg(long, global = true)]
    yolo: bool,
    #[command(subcommand)]
    command: Option<TuiCommand>,
}

#[derive(Debug, Clone, Subcommand)]
enum TuiCommand {
    /// Attach the interactive TUI to a remote app server.
    Remote(RemoteArgs),
    /// Render one deterministic offscreen frame and exit.
    Inspect(InspectArgs),
    /// Run a long-lived NDJSON TUI controller over stdin/stdout.
    Driver(DriverArgs),
}

#[derive(Debug, Clone, ClapArgs)]
struct RemoteArgs {
    /// Root HTTP(S) URL of the Golutra app server.
    #[arg(long, value_name = "URL")]
    url: String,
}

#[derive(Debug, Clone, ClapArgs)]
struct InspectArgs {
    #[arg(long)]
    embedded: bool,
    #[arg(long)]
    session: Option<String>,
    #[arg(long)]
    prompt: Option<String>,
    #[arg(long)]
    wait: Option<String>,
    #[arg(long, default_value_t = 120_000)]
    timeout_ms: u64,
    #[arg(long, default_value_t = 160)]
    width: u16,
    #[arg(long, default_value_t = 40)]
    height: u16,
    #[arg(long)]
    rows: Option<String>,
    #[arg(long, default_value = "response")]
    view: String,
    #[arg(long, default_value = "text")]
    detail: String,
    #[arg(long, default_value = "json")]
    format: String,
}

#[derive(Debug, Clone, ClapArgs)]
struct DriverArgs {
    #[arg(long, conflicts_with = "socket")]
    stdio: bool,
    #[arg(long, value_name = "PATH", conflicts_with = "stdio")]
    socket: Option<PathBuf>,
    #[arg(long)]
    embedded: bool,
    #[arg(long)]
    session: Option<String>,
    #[arg(long, default_value_t = 160)]
    width: u16,
    #[arg(long, default_value_t = 40)]
    height: u16,
    #[arg(long, default_value_t = 900)]
    idle_timeout_secs: u64,
    #[arg(long, default_value_t = 30)]
    heartbeat_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OverlaySurface {
    Help,
    Auth,
    Approval,
    Question,
    Resume,
    Queue,
    Dashboard,
    Settings,
    Export,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RuntimeRefreshBinding {
    session_id: SessionId,
    task_id: Option<TaskId>,
    debug_mode: bool,
}

#[derive(Debug)]
struct RuntimeRefreshSnapshot {
    binding: RuntimeRefreshBinding,
    projection: UserProjection,
    provider_status: Option<ProviderUiStatus>,
    developer_projection: Option<Result<golutra_protocol::DebugProjection, String>>,
    remote: bool,
}

#[derive(Debug, Clone)]
struct TranscriptProjectionAnchor {
    projection: OperationProjection,
    original_index: usize,
    visual_offset: usize,
}

#[derive(Debug)]
struct TuiApp {
    thread_id: ThreadId,
    session_id: SessionId,
    task_id: Option<TaskId>,
    projection: Option<UserProjection>,
    developer_projection: Option<golutra_protocol::DebugProjection>,
    developer_error: Option<String>,
    events: Vec<RuntimeEvent>,
    command_messages: Vec<TranscriptItem>,
    resume_picker: Option<ResumePickerState>,
    queue_picker: Option<QueuePickerState>,
    approval_dialog: Option<ApprovalDialogState>,
    question_dialog: Option<QuestionDialogState>,
    help_dialog: Option<HelpDialogState>,
    dashboard: Option<DashboardState>,
    settings_dialog: Option<SettingsDialogState>,
    editing_queued_turn: Option<TurnId>,
    export_flow: Option<ExportFlowState>,
    export_operation: Option<PendingExportOperation>,
    auth_dialog: Option<AuthDialogState>,
    auth_operation: Option<PendingAuthOperation>,
    input: ComposerInput,
    prompt_history: PromptHistory,
    history_search: Option<HistorySearchState>,
    mention_catalog: MentionCatalog,
    mention_catalog_loaded: bool,
    mention_completion: Option<MentionCompletion>,
    attachments: Vec<ComposerAttachment>,
    selected_attachment: Option<usize>,
    prompt_stash: Option<String>,
    slash_selected: usize,
    status_message: String,
    provider_message: String,
    provider_model: String,
    runtime_controls: RuntimeControls,
    provider_choices: Vec<ProviderChoice>,
    preferences: TuiPreferences,
    preferences_path: Option<PathBuf>,
    release_badge_visible: bool,
    composer_mode: ComposerMode,
    vim_pending_operator: Option<char>,
    terminal_resume_generation: u64,
    mouse_press: Option<UiMousePress>,
    last_activity_refresh_at: Instant,
    workspace_path: PathBuf,
    debug_mode: bool,
    yolo: bool,
    activity_projection: ActivityProjection,
    activity_snapshot: Option<ActivitySnapshot>,
    activity_snapshot_captured: bool,
    change_projection: ChangeProjection,
    expanded_operations: HashSet<OperationId>,
    transcript_details_expanded: bool,
    developer_observations_expanded: bool,
    transcript_scroll: PaneScrollState,
    transcript_top_row_override: Option<usize>,
    transcript_revision: u64,
    transcript_layout_cache: Option<TranscriptLayoutCache>,
    inline_history_enabled: bool,
    inline_history_committed_event_ids: HashSet<EventId>,
    history_replay_generation: u64,
    history_replay_ready: bool,
    body_view_mode: BodyViewMode,
    transcript_presentation: TranscriptPresentation,
    transcript_search: Option<TranscriptSearchState>,
    search_restore_body_view: Option<BodyViewMode>,
    layout: UiLayoutSnapshot,
    cursor: Option<u64>,
    history_start_cursor: Option<u64>,
    history_has_more_before: bool,
    history_load_requested: bool,
    quit_shortcut_expires_at: Option<Instant>,
    should_quit: bool,
    last_prompt_ack: Option<CommandAck>,
    last_control_ack: Option<CommandAck>,
}

async fn load_runtime_refresh_snapshot(
    transport: &RuntimeTransport,
    binding: RuntimeRefreshBinding,
) -> Result<RuntimeRefreshSnapshot, String> {
    let projection = async {
        let value = transport
            .query(RuntimeQuery {
                query_id: QueryId::new(),
                session_id: binding.session_id,
                task_id: binding.task_id,
                kind: RuntimeQueryKind::UserProjection,
                requester: ActorKind::Tui,
                cursor: None,
                timestamp: chrono::Utc::now(),
            })
            .await
            .map_err(|error| error.to_string())?;
        serde_json::from_value(value)
            .map_err(|error| format!("user projection is invalid: {error}"))
    };
    let provider_status = async {
        tokio::time::timeout(
            Duration::from_secs(2),
            provider_ui_status_from_runtime(transport, binding.session_id),
        )
        .await
        .ok()
        .and_then(Result::ok)
    };
    let developer_projection = async {
        if binding.debug_mode {
            Some(load_debug_projection(transport, binding.session_id, binding.task_id).await)
        } else {
            None
        }
    };
    let (projection, provider_status, developer_projection) =
        tokio::join!(projection, provider_status, developer_projection);

    Ok(RuntimeRefreshSnapshot {
        binding,
        projection: projection?,
        provider_status,
        developer_projection,
        remote: transport.is_remote(),
    })
}

impl TuiApp {
    fn overlay_surface_without_help(&self) -> Option<OverlaySurface> {
        if self.auth_dialog.is_some() {
            Some(OverlaySurface::Auth)
        } else if self.approval_dialog.is_some() {
            Some(OverlaySurface::Approval)
        } else if self.question_dialog.is_some() {
            Some(OverlaySurface::Question)
        } else if self.resume_picker.is_some() {
            Some(OverlaySurface::Resume)
        } else if self.queue_picker.is_some() {
            Some(OverlaySurface::Queue)
        } else if self.dashboard.is_some() {
            Some(OverlaySurface::Dashboard)
        } else if self.settings_dialog.is_some() {
            Some(OverlaySurface::Settings)
        } else if self.export_flow.is_some() {
            Some(OverlaySurface::Export)
        } else {
            None
        }
    }

    pub(crate) fn overlay_surface(&self) -> Option<OverlaySurface> {
        self.help_dialog
            .as_ref()
            .map(|_| OverlaySurface::Help)
            .or_else(|| self.overlay_surface_without_help())
    }

    fn new(
        thread_id: ThreadId,
        session_id: SessionId,
        task_id: Option<TaskId>,
        debug_mode: bool,
        provider_message: String,
        auth_dialog: Option<AuthDialogState>,
    ) -> Self {
        let provider_model = provider_model_from_status(&provider_message);
        let (runtime_controls, provider_choices) =
            RuntimeControls::from_settings(None, &provider_model, false);
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
            queue_picker: None,
            approval_dialog: None,
            question_dialog: None,
            help_dialog: None,
            dashboard: None,
            settings_dialog: None,
            editing_queued_turn: None,
            export_flow: None,
            export_operation: None,
            auth_dialog,
            auth_operation: None,
            input: ComposerInput::default(),
            prompt_history: PromptHistory::default(),
            history_search: None,
            mention_catalog: MentionCatalog::default(),
            mention_catalog_loaded: false,
            mention_completion: None,
            attachments: Vec::new(),
            selected_attachment: None,
            prompt_stash: None,
            slash_selected: 0,
            status_message: String::new(),
            provider_message,
            provider_model,
            runtime_controls,
            provider_choices,
            preferences: TuiPreferences::default(),
            preferences_path: None,
            release_badge_visible: false,
            composer_mode: ComposerMode::Standard,
            vim_pending_operator: None,
            terminal_resume_generation: 0,
            mouse_press: None,
            last_activity_refresh_at: Instant::now(),
            workspace_path,
            debug_mode,
            yolo: false,
            activity_projection: ActivityProjection::default(),
            activity_snapshot: None,
            activity_snapshot_captured: false,
            change_projection: ChangeProjection::default(),
            expanded_operations: HashSet::new(),
            transcript_details_expanded: false,
            developer_observations_expanded: debug_mode,
            transcript_scroll: PaneScrollState {
                follow_tail: true,
                ..PaneScrollState::default()
            },
            transcript_top_row_override: None,
            transcript_revision: 0,
            transcript_layout_cache: None,
            inline_history_enabled: false,
            inline_history_committed_event_ids: HashSet::new(),
            history_replay_generation: 0,
            history_replay_ready: true,
            body_view_mode: BodyViewMode::Auto,
            transcript_presentation: TranscriptPresentation::Rich,
            transcript_search: None,
            search_restore_body_view: None,
            layout: UiLayoutSnapshot::default(),
            cursor: None,
            history_start_cursor: None,
            history_has_more_before: false,
            history_load_requested: false,
            quit_shortcut_expires_at: None,
            should_quit: false,
            last_prompt_ack: None,
            last_control_ack: None,
        }
    }

    fn with_footer_context(
        mut self,
        workspace_path: impl Into<PathBuf>,
        provider_model: impl Into<String>,
    ) -> Self {
        self.workspace_path = workspace_path.into();
        self.mention_catalog = MentionCatalog::default();
        self.mention_catalog_loaded = false;
        self.mention_completion = None;
        self.provider_model = provider_model.into();
        let (mut controls, choices) =
            RuntimeControls::from_settings(None, &self.provider_model, self.yolo);
        controls.permission_mode = self.runtime_controls.permission_mode;
        self.runtime_controls = controls;
        self.provider_choices = choices;
        self
    }

    fn with_yolo(mut self, enabled: bool) -> Self {
        self.yolo = enabled;
        self.runtime_controls.permission_mode = if enabled {
            PermissionMode::Unrestricted
        } else {
            PermissionMode::Guarded
        };
        self
    }

    fn with_discovered_runtime_controls(mut self) -> Self {
        let (controls, choices) = RuntimeControls::discover(&self.provider_model, self.yolo);
        self.runtime_controls = controls;
        self.provider_choices = choices;
        self
    }

    fn with_transport_runtime_controls(self, transport: &RuntimeTransport) -> Self {
        if transport.is_remote() {
            self
        } else {
            self.with_discovered_runtime_controls()
        }
    }

    fn with_loaded_preferences(mut self) -> Self {
        match TuiPreferences::global_path() {
            Ok(path) => {
                self.preferences_path = Some(path.clone());
                let preferences = match TuiPreferences::load_from(&path) {
                    Ok(preferences) => preferences,
                    Err(error) => {
                        self.status_message = format!("TUI preferences were not loaded: {error}");
                        return self;
                    }
                };
                self.release_badge_visible = preferences.has_unseen_release();
                self.composer_mode = ComposerMode::for_keymap(preferences.keymap);
                self.preferences = preferences;
                if self.release_badge_visible && self.auth_dialog.is_none() {
                    self.status_message = format!(
                        "Golutra {} is ready; open /whats-new for release notes",
                        env!("CARGO_PKG_VERSION")
                    );
                }
            }
            Err(error) => {
                self.status_message = format!("TUI preferences were not loaded: {error}");
            }
        }
        self
    }

    fn palette(&self) -> TuiPalette {
        self.preferences.palette()
    }

    fn persist_preferences(&mut self) {
        let Some(path) = self.preferences_path.as_deref() else {
            return;
        };
        if let Err(error) = self.preferences.save_to(path) {
            self.status_message = format!("TUI preferences were not saved: {error}");
        }
    }

    fn help_context(&self) -> &'static str {
        match self.overlay_surface_without_help() {
            Some(OverlaySurface::Auth) => "provider setup",
            Some(OverlaySurface::Approval) => "approval",
            Some(OverlaySurface::Question) => "question",
            Some(OverlaySurface::Resume) => "sessions",
            Some(OverlaySurface::Queue) => "queued prompts",
            Some(OverlaySurface::Dashboard) => "runtime dashboard",
            Some(OverlaySurface::Settings) => "settings",
            Some(OverlaySurface::Export) => "session export",
            Some(OverlaySurface::Help) => "help",
            None if self.transcript_search.is_some() => "transcript search",
            None if self.history_search.is_some() => "prompt history search",
            None if self.debug_mode => "developer runtime",
            None => "composer",
        }
    }

    fn open_help(&mut self, topic: HelpTopic) {
        let context = self.help_context();
        self.help_dialog = Some(HelpDialogState::new(topic, context));
        if topic == HelpTopic::WhatsNew {
            self.preferences.mark_current_release_seen();
            self.release_badge_visible = false;
            self.persist_preferences();
        }
        self.status_message = "keyboard reference".to_owned();
    }

    fn refresh_provider_status(&mut self) {
        if let Ok(status) = current_provider_ui_status() {
            self.provider_message = status.message;
            self.provider_model = status.model;
        }
    }

    async fn refresh_provider_status_from_runtime(&mut self, transport: &RuntimeTransport) {
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            provider_ui_status_from_runtime(transport, self.session_id),
        )
        .await;
        if let Ok(Ok(status)) = result {
            self.provider_message = status.message;
            self.provider_model = status.model;
        }
    }

    fn runtime_refresh_binding(&self) -> RuntimeRefreshBinding {
        RuntimeRefreshBinding {
            session_id: self.session_id,
            task_id: self.task_id,
            debug_mode: self.debug_mode,
        }
    }

    fn apply_runtime_refresh_snapshot(&mut self, snapshot: RuntimeRefreshSnapshot) -> bool {
        if snapshot.binding != self.runtime_refresh_binding() {
            return false;
        }

        let previous_row_count = self.transcript_scroll.row_count;
        let projection = Some(snapshot.projection);
        if self.projection != projection {
            self.projection = projection;
            self.invalidate_transcript_layout();
        }
        self.sync_approval_dialog_from_events();
        self.sync_question_dialog_from_events();
        if let Some(status) = snapshot.provider_status {
            self.provider_message = status.message;
            self.provider_model = status.model;
        }
        if self.projection.as_ref().is_some_and(|projection| {
            projection.status == golutra_core::TaskStatus::WaitingAuthentication
        }) && !snapshot.remote
            && self.auth_dialog.is_none()
            && self.auth_operation.is_none()
        {
            self.auth_dialog = Some(AuthDialogState::new());
            self.status_message = "provider authentication required".to_owned();
        }
        if self.debug_mode {
            match snapshot.developer_projection {
                Some(Ok(projection)) => {
                    let mut projection = match self.developer_projection.take() {
                        Some(previous) => merge_debug_projection(previous, projection),
                        None => projection,
                    };
                    if !self.events.is_empty() {
                        replace_debug_event_history(&mut projection, self.events.clone());
                    }
                    self.developer_projection = Some(projection);
                    self.developer_error = None;
                }
                Some(Err(error)) => {
                    self.developer_projection = None;
                    self.developer_error = Some(error);
                }
                None => {}
            }
        } else {
            self.developer_projection = None;
            self.developer_error = None;
        }
        self.refresh_activity_snapshot();
        self.sync_transcript_row_count(previous_row_count);
        true
    }

    async fn refresh(&mut self, transport: &RuntimeTransport) -> miette::Result<()> {
        let snapshot = load_runtime_refresh_snapshot(transport, self.runtime_refresh_binding())
            .await
            .map_err(|error| miette::miette!("{error}"))?;
        self.apply_runtime_refresh_snapshot(snapshot);
        Ok(())
    }

    fn apply_loaded_history(&mut self, history: LoadedEventHistory) {
        self.events = history.events;
        self.invalidate_transcript_layout();
        self.rebuild_event_projections();
        self.history_start_cursor = history.start_cursor;
        self.cursor = history.end_cursor;
        self.history_has_more_before = false;
        self.history_load_requested = false;
        self.sync_transcript_row_count(0);
        self.clamp_transcript_scroll();
        self.history_replay_ready = true;
    }

    fn apply_developer_projection_result(&mut self, result: Result<DebugProjection, String>) {
        match result {
            Ok(mut projection) => {
                if !self.events.is_empty() {
                    replace_debug_event_history(&mut projection, self.events.clone());
                }
                self.developer_projection = Some(projection);
                self.developer_error = None;
            }
            Err(error) => {
                self.developer_projection = None;
                self.developer_error = Some(error.clone());
                self.status_message = format!("runtime observations unavailable: {error}");
            }
        }
    }

    async fn reload_debug_history(&mut self, transport: &RuntimeTransport) -> miette::Result<()> {
        let (history, projection) = tokio::join!(
            load_complete_event_history(transport, self.session_id, self.task_id),
            load_debug_projection(transport, self.session_id, self.task_id),
        );
        let history = history.map_err(|error| miette::miette!("{error}"))?;
        self.begin_history_replay();
        self.apply_loaded_history(history);
        self.apply_developer_projection_result(projection);
        if self.developer_error.is_none() {
            self.status_message = if self.developer_observations_expanded {
                "runtime observations reloaded and expanded"
            } else {
                "runtime observations reloaded in compact form"
            }
            .to_owned();
        }
        Ok(())
    }

    async fn load_recent_history(&mut self, transport: &RuntimeTransport) -> miette::Result<()> {
        let history = load_complete_event_history(transport, self.session_id, self.task_id)
            .await
            .map_err(|error| miette::miette!("{error}"))?;
        self.apply_loaded_history(history);
        Ok(())
    }

    async fn load_older_history(&mut self, transport: &RuntimeTransport) -> miette::Result<()> {
        if !self.history_has_more_before {
            self.history_load_requested = false;
            return Ok(());
        }
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
        let mut older = page.events;
        self.history_load_requested = false;
        if older.is_empty() {
            self.history_has_more_before = false;
            return Ok(());
        }
        older.append(&mut self.events);
        self.events = older;
        self.invalidate_transcript_layout();
        self.rebuild_event_projections();
        self.history_start_cursor = page.start_cursor;
        self.history_has_more_before = page.has_more;
        let current_rows = self.current_transcript_row_count();
        if let Some(top_row) = self.transcript_top_row_override {
            let prepended_rows = current_rows.saturating_sub(self.transcript_scroll.row_count);
            self.transcript_top_row_override = Some(top_row.saturating_add(prepended_rows));
            self.transcript_scroll.row_count = current_rows;
            self.transcript_scroll.follow_tail = false;
        } else {
            self.transcript_scroll
                .set_row_count_after_prepend(current_rows);
        }
        self.clamp_transcript_scroll();
        Ok(())
    }

    fn set_debug_mode(&mut self, enabled: bool) {
        if self.debug_mode == enabled {
            return;
        }
        self.debug_mode = enabled;
        self.body_view_mode = BodyViewMode::Auto;
        self.request_history_rebuild();
        if enabled {
            self.developer_observations_expanded = true;
            self.developer_error = None;
            self.status_message = "developer runtime view visible".to_owned();
        } else {
            self.developer_observations_expanded = false;
            self.developer_projection = None;
            self.developer_error = None;
            self.status_message = "developer runtime view hidden".to_owned();
        }
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
        let event_type = event.event_type;
        let preserve_transcript_anchor = event_type != RuntimeEventType::ProviderStreamed
            && (!self.transcript_scroll.follow_tail || self.transcript_top_row_override.is_some());
        let transcript_anchor = preserve_transcript_anchor
            .then(|| self.first_visible_transcript_anchor())
            .flatten();
        let previous_row_count = self.transcript_scroll.row_count;
        let event_turn_id = event.turn_id;
        match event_type {
            RuntimeEventType::ApprovalRequested => {
                if let Some(request) = event
                    .payload
                    .get("request")
                    .cloned()
                    .and_then(|value| serde_json::from_value::<ApprovalRequest>(value).ok())
                {
                    self.approval_dialog = Some(ApprovalDialogState::new(request));
                    self.status_message = "tool approval required".to_owned();
                }
            }
            RuntimeEventType::ApprovalResolved => {
                self.approval_dialog = None;
            }
            RuntimeEventType::UserQuestionRequested => {
                if let Some(request) = event
                    .payload
                    .get("request")
                    .cloned()
                    .and_then(|value| serde_json::from_value::<UserQuestionRequest>(value).ok())
                {
                    self.question_dialog = Some(QuestionDialogState::new(request));
                    self.status_message = "agent needs a decision".to_owned();
                }
            }
            RuntimeEventType::UserQuestionResolved => {
                self.question_dialog = None;
            }
            event_type if event_type.is_task_terminal() => {
                self.question_dialog = None;
            }
            _ => {}
        }
        self.activity_projection.apply(&event);
        self.change_projection.apply(&event);
        self.events.push(event);
        if matches!(
            event_type,
            RuntimeEventType::TurnStarted | RuntimeEventType::TurnCancelled
        ) && self.editing_queued_turn == event_turn_id
        {
            self.editing_queued_turn = None;
            self.input.reset();
            self.status_message = "queued prompt is no longer pending".to_owned();
        }
        if self.queue_picker.is_some()
            && matches!(
                event_type,
                RuntimeEventType::TurnQueued
                    | RuntimeEventType::TurnUpdated
                    | RuntimeEventType::TurnStarted
                    | RuntimeEventType::TurnCancelled
            )
        {
            let items = queued_prompts(&self.events);
            if items.is_empty() {
                self.queue_picker = None;
            } else if let Some(picker) = &mut self.queue_picker {
                picker.items = items;
                picker.selected = picker.selected.min(picker.items.len().saturating_sub(1));
            }
        }
        self.invalidate_transcript_layout();
        if transcript_anchor.is_some() {
            self.reflow_transcript_with_anchor(transcript_anchor, previous_row_count);
        }
    }

    fn rebuild_event_projections(&mut self) {
        self.activity_projection.rebuild(&self.events);
        self.invalidate_activity_snapshot();
        self.change_projection.rebuild(&self.events);
        self.sync_approval_dialog_from_events();
        self.sync_question_dialog_from_events();
    }

    fn sync_approval_dialog_from_events(&mut self) {
        let pending_id = self
            .projection
            .as_ref()
            .and_then(|projection| projection.pending_approval.as_deref());
        let request = pending_id.and_then(|pending_id| {
            self.events.iter().rev().find_map(|event| {
                (event.event_type == RuntimeEventType::ApprovalRequested
                    && event.payload.get("approval_id").and_then(Value::as_str) == Some(pending_id))
                .then(|| event.payload.get("request").cloned())
                .flatten()
                .and_then(|value| serde_json::from_value::<ApprovalRequest>(value).ok())
            })
        });
        match (request, self.approval_dialog.as_ref()) {
            (Some(request), Some(dialog)) if dialog.request.approval_id == request.approval_id => {}
            (Some(request), _) => {
                self.approval_dialog = Some(ApprovalDialogState::new(request));
            }
            (None, _) => self.approval_dialog = None,
        }
    }

    fn sync_question_dialog_from_events(&mut self) {
        let active_task_id = self.projection.as_ref().and_then(|projection| {
            projection
                .status
                .is_active()
                .then_some(projection.task_id)
                .flatten()
        });
        let request =
            active_task_id.and_then(|task_id| pending_user_question(&self.events, Some(task_id)));
        match (request, self.question_dialog.as_ref()) {
            (Some(request), Some(dialog)) if dialog.request.question_id == request.question_id => {}
            (Some(request), _) => {
                self.question_dialog = Some(QuestionDialogState::new(request));
            }
            (None, _) => self.question_dialog = None,
        }
    }

    fn refresh_activity_snapshot(&mut self) {
        self.refresh_activity_snapshot_at(chrono::Utc::now());
    }

    fn activity_refresh_due(&mut self, now: Instant) -> bool {
        let cadence = if self.preferences.reduced_motion {
            Duration::from_secs(1)
        } else {
            ACTIVITY_STATUS_INTERVAL
        };
        if now.duration_since(self.last_activity_refresh_at) < cadence {
            return false;
        }
        self.last_activity_refresh_at = now;
        true
    }

    fn refresh_activity_snapshot_at(&mut self, now: chrono::DateTime<chrono::Utc>) {
        self.activity_snapshot = self.activity_projection.snapshot(
            self.projection.as_ref().map(|projection| projection.status),
            now,
        );
        self.activity_snapshot_captured = true;
    }

    fn invalidate_activity_snapshot(&mut self) {
        self.activity_snapshot = None;
        self.activity_snapshot_captured = false;
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
        if self.overlay_surface().is_some() {
            self.status_message = "finish the active dialog before submitting".to_owned();
            self.input.clear();
            return Ok(());
        }

        let input = self.input.trimmed();
        if let Some(turn_id) = self.editing_queued_turn {
            return match parse_slash_input(&input) {
                SlashInput::Prompt(prompt) => {
                    self.update_queued_prompt(transport, turn_id, prompt).await
                }
                SlashInput::Empty => {
                    self.status_message = "queued prompt is empty".to_owned();
                    Ok(())
                }
                SlashInput::Command(_) | SlashInput::Error(_) => {
                    self.status_message =
                        "queued prompt editing accepts prompt text only".to_owned();
                    Ok(())
                }
            };
        }
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
            || self.overlay_surface().is_some()
            || self.transcript_search.is_some()
            || self.history_search.is_some()
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

    fn refresh_mention_completion(&mut self) {
        if !mention_is_active(&self.input) {
            self.mention_completion = None;
            return;
        }
        if !self.mention_catalog_loaded {
            self.mention_catalog = MentionCatalog::discover(&self.workspace_path);
            self.mention_catalog_loaded = true;
        }
        self.mention_completion = self.mention_catalog.complete(&self.input);
    }

    fn move_mention_selection(&mut self, forward: bool) -> bool {
        let Some(completion) = &mut self.mention_completion else {
            return false;
        };
        completion.move_selection(forward);
        true
    }

    fn accept_mention_completion(&mut self) -> bool {
        let Some(completion) = self.mention_completion.take() else {
            return false;
        };
        let Some(candidate) = completion.selected().cloned() else {
            return false;
        };
        self.input
            .replace_range(completion.replacement, &format!("{} ", candidate.insertion));
        self.prompt_history.reset_navigation();
        self.status_message = format!("{} reference added", candidate.detail);
        true
    }

    fn previous_prompt(&mut self) -> bool {
        let Some(prompt) = self.prompt_history.previous(self.input.text()) else {
            return false;
        };
        self.input.set_text(prompt);
        self.refresh_mention_completion();
        true
    }

    fn next_prompt(&mut self) -> bool {
        let Some(prompt) = self.prompt_history.next() else {
            return false;
        };
        self.input.set_text(prompt);
        self.refresh_mention_completion();
        true
    }

    fn open_history_search(&mut self) {
        let mut search = HistorySearchState::default();
        search.rebuild(&self.prompt_history);
        self.history_search = Some(search);
        self.mention_completion = None;
        self.status_message = "prompt history search".to_owned();
    }

    fn toggle_prompt_stash(&mut self) {
        if self.input.is_empty() {
            if let Some(stash) = self.prompt_stash.take() {
                self.input.set_text(stash);
                self.status_message = "prompt restored".to_owned();
                self.refresh_mention_completion();
            } else {
                self.status_message = "prompt stash is empty".to_owned();
            }
            return;
        }
        self.prompt_stash = Some(self.input.text().to_owned());
        self.input.reset();
        self.mention_completion = None;
        self.status_message = "prompt stashed".to_owned();
    }

    fn edit_prompt_in_external_editor(&mut self) {
        match edit_prompt_externally(self.input.text()) {
            Ok(prompt) => {
                self.input.set_text(prompt.trim_end_matches(['\r', '\n']));
                self.refresh_mention_completion();
                self.status_message = "prompt updated from external editor".to_owned();
            }
            Err(error) => self.status_message = format!("external editor failed: {error}"),
        }
        self.mark_terminal_resumed();
    }

    fn mark_terminal_resumed(&mut self) {
        self.terminal_resume_generation = self.terminal_resume_generation.saturating_add(1);
    }

    fn terminal_input_stream_is_stale(&self, previous_generation: u64) -> bool {
        self.terminal_resume_generation != previous_generation
    }

    fn add_attachment(&mut self, path: &str) {
        match attachment_from_path(&self.workspace_path, path) {
            Ok(attachment)
                if self
                    .attachments
                    .iter()
                    .any(|existing| existing.path == attachment.path) =>
            {
                self.status_message = "attachment already added".to_owned();
            }
            Ok(attachment) => {
                let display_path = attachment.display_path.clone();
                self.attachments.push(attachment);
                self.selected_attachment = Some(self.attachments.len().saturating_sub(1));
                self.status_message = format!("attached {display_path}");
            }
            Err(error) => self.status_message = error,
        }
    }

    fn clear_attachments(&mut self) {
        self.attachments.clear();
        self.selected_attachment = None;
        self.status_message = "attachments cleared".to_owned();
    }

    fn select_next_attachment(&mut self) {
        if self.attachments.is_empty() {
            self.selected_attachment = None;
            self.status_message = "no prompt attachments".to_owned();
            return;
        }
        self.selected_attachment = Some(
            self.selected_attachment
                .map_or(0, |index| (index + 1) % self.attachments.len()),
        );
        self.status_message = "attachment selected; Delete removes it".to_owned();
    }

    fn remove_selected_attachment(&mut self) -> bool {
        let Some(index) = self
            .selected_attachment
            .filter(|index| *index < self.attachments.len())
        else {
            return false;
        };
        let removed = self.attachments.remove(index);
        self.selected_attachment = if self.attachments.is_empty() {
            None
        } else {
            Some(index.min(self.attachments.len().saturating_sub(1)))
        };
        self.status_message = format!("detached {}", removed.display_path);
        true
    }

    fn open_queue_picker(&mut self) {
        let items = queued_prompts(&self.events);
        if items.is_empty() {
            self.status_message = "queued prompt list is empty".to_owned();
            return;
        }
        self.queue_picker = Some(QueuePickerState { items, selected: 0 });
        self.mention_completion = None;
        self.status_message = "queued prompt manager".to_owned();
    }

    fn open_settings_dialog(&mut self) {
        let runtime_locked = has_active_task(self);
        self.settings_dialog = Some(SettingsDialogState::new(
            &self.runtime_controls,
            &self.provider_choices,
            &self.preferences,
            runtime_locked,
        ));
        self.mention_completion = None;
        self.status_message = if runtime_locked {
            "display settings; runtime controls are locked while the task is active".to_owned()
        } else {
            "session and display settings".to_owned()
        };
    }

    fn apply_settings_dialog(&mut self) -> bool {
        let Some(dialog) = &mut self.settings_dialog else {
            return false;
        };
        if !dialog.can_apply() {
            self.status_message =
                "unrestricted mode disables workspace and approval guards; apply again to confirm"
                    .to_owned();
            return false;
        }
        let previous_keymap = self.preferences.keymap;
        self.runtime_controls = dialog.draft.clone();
        self.preferences = dialog.draft_preferences.clone();
        self.yolo = self.runtime_controls.permission_mode == PermissionMode::Unrestricted;
        self.settings_dialog = None;
        if previous_keymap != self.preferences.keymap {
            self.composer_mode = ComposerMode::for_keymap(self.preferences.keymap);
            self.vim_pending_operator = None;
        }
        self.invalidate_transcript_layout();
        self.persist_preferences();
        self.status_message = format!(
            "settings applied: {} · {} · {} · {} keymap · {} theme",
            self.runtime_controls.effective_model(),
            effort_label(self.runtime_controls.reasoning_effort),
            self.runtime_controls.permission_mode.label(),
            self.preferences.keymap.label(),
            self.preferences.theme.label()
        );
        true
    }

    fn set_session_model(&mut self, model: String) {
        if has_active_task(self) {
            self.status_message = "model is locked while a task is active".to_owned();
            return;
        }
        match self.runtime_controls.set_custom_model(model) {
            Ok(()) => {
                self.status_message = format!(
                    "session model set to {}",
                    self.runtime_controls.effective_model()
                );
            }
            Err(error) => self.status_message = error,
        }
    }

    fn set_session_effort(&mut self, selection: ReasoningEffortSelection) {
        if has_active_task(self) {
            self.status_message = "reasoning effort is locked while a task is active".to_owned();
            return;
        }
        self.runtime_controls.reasoning_effort = match selection {
            ReasoningEffortSelection::Default => None,
            ReasoningEffortSelection::Effort(effort) => Some(effort),
        };
        self.runtime_controls.reasoning_overridden = true;
        self.status_message = format!(
            "session reasoning effort set to {}",
            effort_label(self.runtime_controls.reasoning_effort)
        );
    }

    fn set_permission_mode(&mut self, unrestricted: bool) {
        if has_active_task(self) {
            self.status_message = "permissions are locked while a task is active".to_owned();
            return;
        }
        self.runtime_controls.permission_mode = if unrestricted {
            PermissionMode::Unrestricted
        } else {
            PermissionMode::Guarded
        };
        self.yolo = unrestricted;
        self.status_message = format!(
            "session permissions set to {}",
            self.runtime_controls.permission_mode.label()
        );
    }

    fn edit_selected_queued_prompt(&mut self) {
        let Some(queued) = self
            .queue_picker
            .as_ref()
            .and_then(QueuePickerState::selected)
            .cloned()
        else {
            return;
        };
        self.queue_picker = None;
        self.editing_queued_turn = Some(queued.turn_id);
        self.input.set_text(queued.prompt);
        self.prompt_history.reset_navigation();
        self.refresh_mention_completion();
        self.status_message = "editing queued prompt; Enter updates, Esc cancels edit".to_owned();
    }

    async fn cancel_selected_queued_prompt(
        &mut self,
        transport: &RuntimeTransport,
    ) -> miette::Result<()> {
        let Some(turn_id) = self
            .queue_picker
            .as_ref()
            .and_then(QueuePickerState::selected)
            .map(|queued| queued.turn_id)
        else {
            return Ok(());
        };
        let ack = transport
            .send_command(session_command(
                self.session_id,
                SessionCommandKind::CancelQueuedTurn,
                json!({"turn_id": turn_id}),
            ))
            .await
            .map_err(|error| miette::miette!("{error}"))?;
        self.status_message = compact_ack_reason(&ack.reason);
        self.last_control_ack = Some(ack.clone());
        if ack.accepted {
            self.refresh(transport).await?;
            let items = queued_prompts(&self.events);
            if items.is_empty() {
                self.queue_picker = None;
            } else if let Some(picker) = &mut self.queue_picker {
                picker.items = items;
                picker.selected = picker.selected.min(picker.items.len().saturating_sub(1));
            }
        }
        Ok(())
    }

    async fn update_queued_prompt(
        &mut self,
        transport: &RuntimeTransport,
        turn_id: TurnId,
        prompt: String,
    ) -> miette::Result<()> {
        let mut payload = json!({
            "turn_id": turn_id,
            "prompt": prompt,
        });
        if !self.attachments.is_empty() {
            payload["attachments"] = Value::Array(
                self.attachments
                    .iter()
                    .map(|attachment| {
                        json!({
                            "path": attachment.display_path,
                            "kind": match attachment.kind {
                                AttachmentKind::Image => "image",
                                AttachmentKind::Text => "text",
                                AttachmentKind::Binary => "binary",
                            },
                            "bytes": attachment.bytes,
                        })
                    })
                    .collect(),
            );
        }
        let ack = transport
            .send_command(session_command(
                self.session_id,
                SessionCommandKind::UpdateQueuedTurn,
                payload,
            ))
            .await
            .map_err(|error| miette::miette!("{error}"))?;
        self.status_message = compact_ack_reason(&ack.reason);
        self.last_prompt_ack = Some(ack.clone());
        if ack.accepted {
            self.prompt_history.record(&prompt);
            self.editing_queued_turn = None;
            self.input.reset();
            self.attachments.clear();
            self.selected_attachment = None;
            self.mention_completion = None;
            self.refresh(transport).await?;
        }
        Ok(())
    }

    fn reset_transcript_view(&mut self) {
        self.expanded_operations.clear();
        self.transcript_details_expanded = false;
        self.transcript_top_row_override = None;
        self.invalidate_transcript_layout();
        self.transcript_scroll
            .reset(self.current_transcript_row_count());
    }

    fn toggle_operation(&mut self, id: OperationId) {
        let anchor = self.first_visible_transcript_anchor();
        let previous_row_count = self.transcript_scroll.row_count;
        if !self.expanded_operations.insert(id.clone()) {
            self.expanded_operations.remove(&id);
        }
        self.invalidate_transcript_layout();
        self.reflow_transcript_with_anchor(anchor, previous_row_count);
    }

    fn toggle_transcript_details(&mut self) {
        let anchor = self.first_visible_transcript_anchor();
        let previous_row_count = self.transcript_scroll.row_count;
        self.transcript_details_expanded = !self.transcript_details_expanded;
        self.invalidate_transcript_layout();
        self.reflow_transcript_with_anchor(anchor, previous_row_count);
        self.status_message = if self.transcript_details_expanded {
            "transcript details expanded"
        } else {
            "transcript details collapsed"
        }
        .to_owned();
    }

    fn reset_history_window(&mut self) {
        self.cursor = None;
        self.history_start_cursor = None;
        self.history_has_more_before = false;
        self.history_load_requested = false;
    }

    fn sync_transcript_row_count(&mut self, previous_row_count: usize) {
        let current_row_count = self.current_transcript_row_count();
        self.sync_transcript_row_count_to(previous_row_count, current_row_count);
    }

    fn current_transcript_row_count(&self) -> usize {
        transcript_layout(self, self.layout.transcript).row_count
    }

    #[cfg(test)]
    fn first_visible_transcript_projection(&self) -> Option<usize> {
        self.first_visible_transcript_anchor()
            .map(|anchor| anchor.original_index)
    }

    fn first_visible_transcript_anchor(&self) -> Option<TranscriptProjectionAnchor> {
        let area = self.layout.transcript;
        if area.width == 0 || area.height == 0 {
            return None;
        }
        let layout = transcript_layout(self, area);
        let window = layout.visible_window(
            area.height as usize,
            self.transcript_scroll.offset_from_bottom,
            self.transcript_top_row_override,
        );
        let projection_index = layout.first_visible_projection(window.clone())?;
        let visual_start = layout.visual_start_for_projection(projection_index)?;
        let projections = rendered_transcript_operation_projections(self);
        Some(TranscriptProjectionAnchor {
            projection: projections.get(projection_index)?.clone(),
            original_index: projection_index,
            visual_offset: window.start.saturating_sub(visual_start),
        })
    }

    fn reflow_transcript_with_anchor(
        &mut self,
        anchor: Option<TranscriptProjectionAnchor>,
        previous_row_count: usize,
    ) {
        let area = self.layout.transcript;
        let layout = transcript_layout(self, area);
        let projections = rendered_transcript_operation_projections(self);
        if let Some(top_row) = anchor.and_then(|anchor| {
            let projection_index = projections
                .iter()
                .enumerate()
                .filter(|(_, projection)| **projection == anchor.projection)
                .min_by_key(|(index, _)| index.abs_diff(anchor.original_index))
                .map(|(index, _)| index)
                .or_else(|| {
                    let id = anchor.projection.id()?;
                    projections
                        .iter()
                        .position(|projection| projection.id() == Some(id))
                })
                .or_else(|| {
                    (!projections.is_empty()).then(|| {
                        anchor
                            .original_index
                            .min(projections.len().saturating_sub(1))
                    })
                })?;
            let range = layout.visual_range_for_projection(projection_index)?;
            Some(
                range.start.saturating_add(
                    anchor
                        .visual_offset
                        .min(range.end.saturating_sub(range.start).saturating_sub(1)),
                ),
            )
        }) {
            self.transcript_scroll.row_count = layout.row_count;
            self.set_transcript_top_row(&layout, top_row, area.height as usize);
        } else {
            self.sync_transcript_row_count_to(previous_row_count, layout.row_count);
        }
        self.transcript_layout_cache = Some(TranscriptLayoutCache {
            revision: self.transcript_revision,
            width: area.width,
            height: area.height,
            layout,
        });
    }

    fn set_transcript_top_row(
        &mut self,
        layout: &TranscriptLayout,
        top_row: usize,
        visible_rows: usize,
    ) {
        if layout.row_count == 0 {
            self.transcript_scroll.reset(0);
            self.transcript_top_row_override = None;
            return;
        }
        let top_row = top_row.min(layout.row_count.saturating_sub(1));
        let visible_rows = visible_rows.max(1);
        let normal_max_start = layout.row_count.saturating_sub(visible_rows);
        if top_row > normal_max_start {
            self.transcript_scroll.offset_from_bottom = 0;
            self.transcript_scroll.follow_tail = false;
            self.transcript_top_row_override = Some(top_row);
        } else {
            let offset_from_bottom = normal_max_start.saturating_sub(top_row);
            self.transcript_scroll.offset_from_bottom = offset_from_bottom;
            self.transcript_scroll.follow_tail = offset_from_bottom == 0;
            self.transcript_top_row_override = None;
            self.transcript_scroll.clamp(visible_rows);
        }
    }

    fn invalidate_transcript_layout(&mut self) {
        self.transcript_revision = self.transcript_revision.wrapping_add(1);
        self.transcript_layout_cache = None;
    }

    fn enable_inline_history(&mut self) {
        if !self.inline_history_enabled {
            self.inline_history_enabled = true;
            self.invalidate_transcript_layout();
        }
    }

    fn begin_history_replay(&mut self) {
        self.history_replay_generation = self.history_replay_generation.wrapping_add(1);
        self.history_replay_ready = false;
        self.set_inline_history_committed_event_ids(HashSet::new());
    }

    fn request_history_rebuild(&mut self) {
        self.history_replay_generation = self.history_replay_generation.wrapping_add(1);
        self.set_inline_history_committed_event_ids(HashSet::new());
    }

    fn set_inline_history_committed_event_ids(&mut self, ids: HashSet<EventId>) {
        if self.inline_history_committed_event_ids != ids {
            self.inline_history_committed_event_ids = ids;
            self.invalidate_transcript_layout();
        }
    }

    fn ensure_transcript_layout(&mut self, area: ratatui::layout::Rect) {
        let stale = self.transcript_layout_cache.as_ref().is_none_or(|cache| {
            cache.revision != self.transcript_revision
                || cache.width != area.width
                || cache.height != area.height
        });
        if stale {
            let resize_anchor = self.transcript_layout_cache.as_ref().and_then(|cache| {
                (cache.revision == self.transcript_revision
                    && (cache.width != area.width || cache.height != area.height)
                    && cache.height > 0
                    && (!self.transcript_scroll.follow_tail
                        || self.transcript_top_row_override.is_some()))
                .then(|| {
                    let window = cache.layout.visible_window(
                        cache.height as usize,
                        self.transcript_scroll.offset_from_bottom,
                        self.transcript_top_row_override,
                    );
                    cache.layout.first_visible_row_anchor(window)
                })
                .flatten()
            });
            let previous_row_count = self.transcript_scroll.row_count;
            let layout = transcript_layout(self, area);
            if let Some(top_row) = resize_anchor
                .and_then(|(row_index, offset)| layout.visual_row_for_row_anchor(row_index, offset))
            {
                self.transcript_scroll.row_count = layout.row_count;
                self.set_transcript_top_row(&layout, top_row, area.height as usize);
            } else {
                self.sync_transcript_row_count_to(previous_row_count, layout.row_count);
            }
            self.transcript_layout_cache = Some(TranscriptLayoutCache {
                revision: self.transcript_revision,
                width: area.width,
                height: area.height,
                layout,
            });
        }
    }

    fn sync_transcript_row_count_to(
        &mut self,
        previous_row_count: usize,
        current_row_count: usize,
    ) {
        if self
            .transcript_top_row_override
            .is_some_and(|top_row| top_row < current_row_count)
        {
            if current_row_count > previous_row_count {
                self.transcript_scroll.unseen_rows = self
                    .transcript_scroll
                    .unseen_rows
                    .saturating_add(current_row_count - previous_row_count);
            }
            self.transcript_scroll.row_count = current_row_count;
            self.transcript_scroll.follow_tail = false;
            return;
        }
        self.transcript_top_row_override = None;
        if !self.transcript_scroll.follow_tail && current_row_count > previous_row_count {
            let added = current_row_count - previous_row_count;
            self.transcript_scroll.offset_from_bottom = self
                .transcript_scroll
                .offset_from_bottom
                .saturating_add(added);
            self.transcript_scroll.unseen_rows =
                self.transcript_scroll.unseen_rows.saturating_add(added);
        }
        self.transcript_scroll.row_count = current_row_count;
        self.clamp_transcript_scroll();
    }

    fn scroll_transcript(&mut self, action: TranscriptScrollAction, visible_rows: usize) {
        if self.overlay_surface().is_some() {
            return;
        }
        if let Some(top_row) = self.transcript_top_row_override {
            match action {
                TranscriptScrollAction::Top | TranscriptScrollAction::Bottom => {
                    self.transcript_top_row_override = None;
                    self.transcript_scroll.scroll(action, visible_rows);
                }
                TranscriptScrollAction::LineUp
                | TranscriptScrollAction::LineDown
                | TranscriptScrollAction::PageUp
                | TranscriptScrollAction::PageDown => {
                    let page = visible_rows.max(1);
                    let max_top = self
                        .transcript_scroll
                        .row_count
                        .saturating_sub(visible_rows.max(1));
                    let next_top = match action {
                        TranscriptScrollAction::LineUp => top_row.saturating_sub(1),
                        TranscriptScrollAction::LineDown => top_row.saturating_add(1).min(max_top),
                        TranscriptScrollAction::PageUp => top_row.saturating_sub(page),
                        TranscriptScrollAction::PageDown => {
                            top_row.saturating_add(page).min(max_top)
                        }
                        TranscriptScrollAction::Top | TranscriptScrollAction::Bottom => {
                            unreachable!()
                        }
                    };
                    let layout = transcript_layout(self, self.layout.transcript);
                    self.set_transcript_top_row(&layout, next_top, visible_rows);
                }
            }
        } else {
            self.transcript_scroll.scroll(action, visible_rows);
        }
        if matches!(
            action,
            TranscriptScrollAction::LineDown
                | TranscriptScrollAction::PageDown
                | TranscriptScrollAction::Bottom
        ) {
            self.history_load_requested = false;
        } else if !self.inline_history_enabled
            && self.history_has_more_before
            && matches!(
                action,
                TranscriptScrollAction::LineUp
                    | TranscriptScrollAction::PageUp
                    | TranscriptScrollAction::Top
            )
            && self.transcript_top_row_override.map_or_else(
                || {
                    self.transcript_scroll.offset_from_bottom
                        == self.max_transcript_scroll_offset(visible_rows)
                },
                |top_row| top_row == 0,
            )
        {
            self.history_load_requested = true;
        }
        self.status_message = transcript_scroll_status(
            self.transcript_scroll.offset_from_bottom,
            self.transcript_scroll.unseen_rows,
        );
    }

    fn clamp_transcript_scroll(&mut self) {
        self.transcript_scroll.clamp(usize::MAX);
    }

    fn max_transcript_scroll_offset(&self, visible_rows: usize) -> usize {
        self.transcript_scroll.max_offset(visible_rows)
    }

    fn toggle_transcript_fullscreen(&mut self) {
        self.body_view_mode = if self.body_view_mode == BodyViewMode::Transcript {
            BodyViewMode::Auto
        } else {
            BodyViewMode::Transcript
        };
        if self.debug_mode {
            self.request_history_rebuild();
        }
        self.status_message = if self.body_view_mode == BodyViewMode::Transcript {
            "transcript view"
        } else {
            "developer event view"
        }
        .to_owned();
    }

    fn toggle_raw_transcript(&mut self) {
        self.transcript_presentation = match self.transcript_presentation {
            TranscriptPresentation::Rich => TranscriptPresentation::Raw,
            TranscriptPresentation::Raw => TranscriptPresentation::Rich,
        };
        self.body_view_mode = BodyViewMode::Transcript;
        if self.debug_mode {
            self.request_history_rebuild();
        }
        self.status_message = match self.transcript_presentation {
            TranscriptPresentation::Rich => "rich transcript view",
            TranscriptPresentation::Raw => "raw transcript view",
        }
        .to_owned();
    }

    fn open_transcript_search(&mut self) {
        if self.transcript_search.is_none() {
            self.search_restore_body_view = Some(self.body_view_mode);
            self.transcript_search = Some(TranscriptSearchState::default());
        }
        self.body_view_mode = BodyViewMode::Transcript;
        if self.debug_mode {
            self.request_history_rebuild();
        }
        self.rebuild_transcript_search();
        self.status_message = "transcript search".to_owned();
    }

    fn close_transcript_search(&mut self) {
        self.transcript_search = None;
        self.body_view_mode = self.search_restore_body_view.take().unwrap_or_default();
        if self.debug_mode {
            self.request_history_rebuild();
        }
        self.status_message = "transcript search closed".to_owned();
    }

    fn rebuild_transcript_search(&mut self) {
        let area = if self.layout.body.width > 0 {
            self.layout.body
        } else {
            self.layout.transcript
        };
        let layout = transcript_layout(self, area);
        let lines = layout.plain_lines();
        if let Some(search) = &mut self.transcript_search {
            search.rebuild(&lines);
        }
        self.focus_current_search_match_in(&layout, area.height as usize);
    }

    fn focus_current_search_match(&mut self) {
        let area = if self.layout.body.width > 0 {
            self.layout.body
        } else {
            self.layout.transcript
        };
        let layout = transcript_layout(self, area);
        self.focus_current_search_match_in(&layout, area.height as usize);
    }

    fn focus_current_search_match_in(&mut self, layout: &TranscriptLayout, visible_rows: usize) {
        let Some(line) = self
            .transcript_search
            .as_ref()
            .and_then(TranscriptSearchState::current_line)
        else {
            return;
        };
        let Some(target) = layout.visual_start_for_line(line) else {
            return;
        };
        let visible_rows = visible_rows.max(1);
        let top = target.saturating_sub(visible_rows / 3);
        let end = top.saturating_add(visible_rows).min(layout.row_count);
        self.transcript_scroll.offset_from_bottom = layout.row_count.saturating_sub(end);
        self.transcript_scroll.follow_tail = self.transcript_scroll.offset_from_bottom == 0;
        self.transcript_scroll.row_count = layout.row_count;
    }

    fn copy_transcript(&mut self) {
        let area = if self.layout.body.width > 0 {
            self.layout.body
        } else {
            self.layout.transcript
        };
        let layout = full_transcript_layout(self, area);
        let lines = layout.plain_lines();
        let text = self
            .transcript_search
            .as_ref()
            .and_then(TranscriptSearchState::current_line)
            .and_then(|line| lines.get(line).cloned())
            .unwrap_or_else(|| layout.plain_text());
        self.status_message = match copy_to_terminal_clipboard(&text) {
            Ok((bytes, true)) => format!("copied {bytes} bytes (clipboard limit reached)"),
            Ok((bytes, false)) => format!("copied {bytes} bytes"),
            Err(error) => format!("copy failed: {error}"),
        };
    }

    fn scroll_active_pane(&mut self, action: TranscriptScrollAction) {
        if self.debug_mode {
            return;
        }
        let rows = self.layout.transcript.height.max(1) as usize;
        self.scroll_transcript(action, rows);
    }

    async fn send_runtime_prompt(
        &mut self,
        transport: &RuntimeTransport,
        prompt: String,
    ) -> miette::Result<()> {
        self.submit_runtime_prompt_with_mode(transport, prompt, false)
            .await
            .map(drop)
    }

    async fn send_steering_prompt(
        &mut self,
        transport: &RuntimeTransport,
        prompt: String,
    ) -> miette::Result<()> {
        self.submit_runtime_prompt_with_mode(transport, prompt, true)
            .await
            .map(drop)
    }

    async fn submit_runtime_prompt(
        &mut self,
        transport: &RuntimeTransport,
        prompt: String,
    ) -> miette::Result<Option<CommandAck>> {
        self.submit_runtime_prompt_with_mode(transport, prompt, false)
            .await
    }

    async fn submit_runtime_prompt_with_mode(
        &mut self,
        transport: &RuntimeTransport,
        prompt: String,
        steer: bool,
    ) -> miette::Result<Option<CommandAck>> {
        self.last_prompt_ack = None;
        if prompt.trim().is_empty() {
            self.status_message = "prompt is empty".to_owned();
            return Ok(None);
        }

        let payload = if steer {
            self.runtime_prompt_payload_with_mode(prompt.clone(), true)
        } else {
            self.runtime_prompt_payload(prompt.clone())
        };
        let ack = transport
            .send_command(session_command(
                self.session_id,
                SessionCommandKind::Prompt,
                payload,
            ))
            .await
            .map_err(|error| miette::miette!("{error}"))?;
        if !ack.accepted {
            self.status_message = compact_ack_reason(&ack.reason);
            self.last_prompt_ack = Some(ack.clone());
            return Ok(Some(ack));
        }
        self.prompt_history.record(&prompt);
        self.input.reset();
        self.mention_completion = None;
        self.attachments.clear();
        self.selected_attachment = None;
        self.status_message = compact_ack_reason(&ack.reason);
        self.reset_transcript_view();
        self.refresh(transport).await?;
        self.last_prompt_ack = Some(ack.clone());
        Ok(Some(ack))
    }

    fn runtime_prompt_payload(&self, prompt: String) -> serde_json::Value {
        self.runtime_prompt_payload_with_mode(prompt, false)
    }

    fn runtime_prompt_payload_with_mode(&self, prompt: String, steer: bool) -> serde_json::Value {
        let mut payload = json!({
            "prompt": prompt,
            "_thread_id": self.thread_id.to_string(),
        });
        if steer {
            payload["steer"] = json!(true);
        }
        if !self.attachments.is_empty() {
            payload["attachments"] = Value::Array(
                self.attachments
                    .iter()
                    .map(|attachment| {
                        json!({
                            "path": attachment.display_path,
                            "kind": match attachment.kind {
                                AttachmentKind::Image => "image",
                                AttachmentKind::Text => "text",
                                AttachmentKind::Binary => "binary",
                            },
                            "bytes": attachment.bytes,
                        })
                    })
                    .collect(),
            );
        }
        if self.yolo {
            payload["yolo"] = json!(true);
            payload["allow_network"] = json!(true);
        }
        if let Some(profile_name) = &self.runtime_controls.profile_name {
            payload["provider_profile"] = json!(profile_name);
        }
        if let Some(model_id) = &self.runtime_controls.custom_model {
            payload["provider_model"] = json!(model_id);
        }
        if let Some(generation_config) = self.runtime_controls.generation_override() {
            payload["provider_generation_config"] = json!(generation_config);
        }
        payload
    }

    fn take_last_prompt_ack(&mut self) -> Option<CommandAck> {
        self.last_prompt_ack.take()
    }

    fn take_last_control_ack(&mut self) -> Option<CommandAck> {
        self.last_control_ack.take()
    }

    async fn execute_slash_command(
        &mut self,
        transport: &RuntimeTransport,
        command: SlashCommand,
    ) -> miette::Result<()> {
        self.last_control_ack = None;
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
                if transport.is_remote() {
                    self.push_system_message(
                        "Auth",
                        vec![
                            "remote TUI cannot write provider credentials".to_owned(),
                            "configure the app-server host, then use /auth status".to_owned(),
                        ],
                    );
                } else {
                    self.open_auth_dialog();
                }
            }
            SlashCommand::Help => self.open_help(HelpTopic::Overview),
            SlashCommand::WhatsNew => self.open_help(HelpTopic::WhatsNew),
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
                self.begin_history_replay();
                self.task_id = None;
                self.projection = None;
                self.developer_projection = None;
                self.developer_error = None;
                self.events.clear();
                self.activity_projection = ActivityProjection::default();
                self.invalidate_activity_snapshot();
                self.change_projection = ChangeProjection::default();
                self.command_messages.clear();
                self.input.reset();
                self.mention_completion = None;
                self.attachments.clear();
                self.selected_attachment = None;
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
                        format!(
                            "provider {} · effort {} · permissions {}",
                            self.runtime_controls.effective_model(),
                            effort_label(self.runtime_controls.reasoning_effort),
                            self.runtime_controls.permission_mode.label()
                        ),
                    ],
                );
            }
            SlashCommand::Plan => {
                self.dashboard = Some(DashboardState::new(DashboardTab::Plan));
                self.status_message = "execution plan".to_owned();
            }
            SlashCommand::Tasks => {
                self.dashboard = Some(DashboardState::new(DashboardTab::Tasks));
                self.status_message = "task activity".to_owned();
            }
            SlashCommand::Usage => {
                self.dashboard = Some(DashboardState::new(DashboardTab::Usage));
                self.status_message = "runtime usage".to_owned();
            }
            SlashCommand::Terminal { command } => {
                if transport.is_remote() {
                    self.status_message =
                        "the interactive shell is unavailable for remote runtimes".to_owned();
                } else if has_active_task(self) {
                    self.status_message =
                        "the interactive shell is unavailable while a task is active".to_owned();
                } else {
                    match run_terminal_session(command.as_deref()) {
                        Ok(()) => self.status_message = "terminal session closed".to_owned(),
                        Err(error) => {
                            self.status_message = format!("terminal session failed: {error}");
                        }
                    }
                    self.mark_terminal_resumed();
                }
            }
            SlashCommand::Settings => self.open_settings_dialog(),
            SlashCommand::Model { model } => match model {
                Some(model) => self.set_session_model(model),
                None => self.open_settings_dialog(),
            },
            SlashCommand::Effort { effort } => match effort {
                Some(effort) => self.set_session_effort(effort),
                None => self.open_settings_dialog(),
            },
            SlashCommand::Permissions { unrestricted } => match unrestricted {
                Some(unrestricted) => self.set_permission_mode(unrestricted),
                None => self.open_settings_dialog(),
            },
            SlashCommand::Debug(action) => {
                if action == SlashDebugCommand::Off {
                    self.set_debug_mode(false);
                } else {
                    let expanded = match action {
                        SlashDebugCommand::Toggle => {
                            !self.debug_mode || !self.developer_observations_expanded
                        }
                        SlashDebugCommand::Expand => true,
                        SlashDebugCommand::Compact => false,
                        SlashDebugCommand::Off => unreachable!("handled above"),
                    };
                    self.set_debug_mode(true);
                    self.body_view_mode = BodyViewMode::Auto;
                    self.developer_observations_expanded = expanded;
                    self.reload_debug_history(transport).await?;
                }
            }
            SlashCommand::Takeover => {
                let ack = self
                    .send_control_command(transport, SessionCommandKind::Takeover)
                    .await?;
                self.last_control_ack = Some(ack);
            }
            SlashCommand::Abort => {
                let ack = self.abort(transport).await?;
                self.last_control_ack = Some(ack);
            }
            SlashCommand::Pause => {
                let ack = self
                    .send_control_command(transport, SessionCommandKind::Pause)
                    .await?;
                self.last_control_ack = Some(ack);
            }
            SlashCommand::Continue => {
                let ack = self
                    .send_control_command(transport, SessionCommandKind::Resume)
                    .await?;
                self.last_control_ack = Some(ack);
            }
            SlashCommand::Approve => {
                let ack = self
                    .resolve_pending_approval(transport, SessionCommandKind::Approve)
                    .await?;
                self.last_control_ack = Some(ack);
            }
            SlashCommand::Deny => {
                let ack = self
                    .resolve_pending_approval(transport, SessionCommandKind::Deny)
                    .await?;
                self.last_control_ack = Some(ack);
            }
            SlashCommand::Compact => {
                let ack = self
                    .send_control_command(transport, SessionCommandKind::Compact)
                    .await?;
                self.last_control_ack = Some(ack);
            }
            SlashCommand::Queue => self.open_queue_picker(),
            SlashCommand::Attach { path } => self.add_attachment(&path),
            SlashCommand::Detach => self.clear_attachments(),
            SlashCommand::Editor => self.edit_prompt_in_external_editor(),
            SlashCommand::Stash => self.toggle_prompt_stash(),
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
        self.queue_picker = None;
        self.editing_queued_turn = None;
        self.resume_picker = Some(ResumePickerState::new(items));
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
        self.queue_picker = None;
        self.approval_dialog = None;
        self.question_dialog = None;
        self.help_dialog = None;
        self.dashboard = None;
        self.editing_queued_turn = None;
        self.export_flow = Some(ExportFlowState {
            picker: ResumePickerState::new(items),
            step: ExportFlowStep::SelectSession,
            range_input: ComposerInput::from_text("1"),
            destination_input: ComposerInput::default(),
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

    fn export_input_mut(&mut self) -> Option<&mut ComposerInput> {
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
                if let Err(error) = parse_session_range(flow.range_input.text()) {
                    flow.error = Some(error.to_string());
                    self.status_message = "invalid export range".to_owned();
                } else {
                    flow.step = ExportFlowStep::Destination;
                    self.status_message = "enter an absolute export directory".to_owned();
                }
            }
            ExportFlowStep::Destination => {
                if !Path::new(flow.destination_input.text()).is_absolute() {
                    flow.error = Some("destination must be an absolute path".to_owned());
                    self.status_message = "absolute path required".to_owned();
                } else if flow.destination_input.trimmed().is_empty() {
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
                let range = match parse_session_range(flow.range_input.text()) {
                    Ok(range) => range,
                    Err(error) => {
                        flow.error = Some(error.to_string());
                        flow.step = ExportFlowStep::Error;
                        return Ok(());
                    }
                };
                let destination = PathBuf::from(flow.destination_input.text());
                flow.step = ExportFlowStep::Running;
                flow.error = None;
                let transport = transport.clone();
                let task = tokio::spawn(async move {
                    DebugExportCoordinator::new(&transport)
                        .export(DebugExportRequest {
                            selection: golutra_protocol::SessionWindowRequest {
                                anchor_thread_id: thread_id,
                                range,
                            },
                            destination,
                        })
                        .await
                        .map_err(|error| error.to_string())
                });
                self.export_operation = Some(PendingExportOperation { task });
                self.status_message = "export running".to_owned();
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

    async fn poll_export_operation(&mut self) {
        if !self
            .export_operation
            .as_ref()
            .is_some_and(|operation| operation.task.is_finished())
        {
            return;
        }
        let Some(operation) = self.export_operation.take() else {
            return;
        };
        let result = match operation.task.await {
            Ok(result) => result,
            Err(error) => Err(format!("export task failed: {error}")),
        };
        let Some(flow) = self.export_flow.as_mut() else {
            return;
        };
        match result {
            Ok(receipt) => {
                self.status_message = "export complete".to_owned();
                flow.receipt = Some(receipt);
                flow.step = ExportFlowStep::Completed;
            }
            Err(error) => {
                self.status_message = "export failed".to_owned();
                flow.error = Some(error);
                flow.step = ExportFlowStep::Error;
            }
        }
    }

    fn start_new_session(&mut self) {
        self.thread_id = ThreadId::new();
        self.session_id = SessionId::new();
        self.begin_history_replay();
        self.task_id = None;
        self.projection = None;
        self.developer_projection = None;
        self.developer_error = None;
        self.events.clear();
        self.activity_projection = ActivityProjection::default();
        self.invalidate_activity_snapshot();
        self.change_projection = ChangeProjection::default();
        self.command_messages.clear();
        self.input.reset();
        self.mention_completion = None;
        self.attachments.clear();
        self.selected_attachment = None;
        self.export_flow = None;
        self.reset_slash_selection();
        self.reset_history_window();
        self.resume_picker = None;
        self.queue_picker = None;
        self.approval_dialog = None;
        self.question_dialog = None;
        self.help_dialog = None;
        self.dashboard = None;
        self.editing_queued_turn = None;
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
        self.begin_history_replay();
        self.task_id = None;
        self.projection = None;
        self.developer_projection = None;
        self.developer_error = None;
        self.events.clear();
        self.activity_projection = ActivityProjection::default();
        self.invalidate_activity_snapshot();
        self.change_projection = ChangeProjection::default();
        self.command_messages.clear();
        self.input.reset();
        self.mention_completion = None;
        self.attachments.clear();
        self.selected_attachment = None;
        self.export_flow = None;
        self.reset_slash_selection();
        self.reset_history_window();
        self.resume_picker = None;
        self.queue_picker = None;
        self.approval_dialog = None;
        self.question_dialog = None;
        self.dashboard = None;
        self.editing_queued_turn = None;
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

    async fn apply_session_picker_action(
        &mut self,
        transport: &RuntimeTransport,
    ) -> miette::Result<()> {
        let Some(picker) = self.resume_picker.as_ref() else {
            return Ok(());
        };
        let Some(item) = picker.items.get(picker.selected) else {
            self.status_message = "no session selected".to_owned();
            return Ok(());
        };
        let Some(action) = picker.action else {
            return Ok(());
        };
        let thread_id = item.thread_id;
        let title = picker.action_input.trimmed();
        let (kind, payload) = match action {
            SessionPickerAction::Rename => (
                SessionCommandKind::RenameThread,
                json!({"thread_id": thread_id, "title": title}),
            ),
            SessionPickerAction::Archive => (
                SessionCommandKind::ArchiveThread,
                json!({"thread_id": thread_id}),
            ),
            SessionPickerAction::Delete => (
                SessionCommandKind::DeleteThread,
                json!({"thread_id": thread_id}),
            ),
        };
        let ack = transport
            .send_command(session_command(self.session_id, kind, payload))
            .await
            .map_err(|error| miette::miette!("{error}"))?;
        self.status_message = compact_ack_reason(&ack.reason);
        if !ack.accepted {
            return Ok(());
        }
        if let Some(picker) = &mut self.resume_picker {
            match action {
                SessionPickerAction::Rename => picker.rename_selected(&title),
                SessionPickerAction::Archive | SessionPickerAction::Delete => {
                    picker.remove_selected();
                }
            }
            picker.finish_action();
            if picker.items.is_empty() {
                self.resume_picker = None;
            }
        }
        self.last_control_ack = Some(ack);
        Ok(())
    }

    fn move_resume_selection(&mut self, direction: ResumeSelectionDirection) {
        if let Some(picker) = &mut self.resume_picker {
            picker.move_selection(direction);
        }
    }

    fn move_overlay_selection(&mut self, direction: ResumeSelectionDirection) {
        let surface = self.overlay_surface();
        let help_max_scroll = self.help_dialog.as_ref().map_or(0, |dialog| {
            help_scroll_max(dialog, self, self.layout.transcript)
        });
        let auth_max_scroll = self
            .auth_dialog
            .as_ref()
            .map_or(0, |dialog| auth_scroll_max(dialog, self.layout.transcript));
        let next = matches!(direction, ResumeSelectionDirection::Next);
        match surface {
            Some(OverlaySurface::Help) => {
                self.help_dialog
                    .as_mut()
                    .expect("help surface")
                    .scroll_by(if next { 1 } else { -1 }, help_max_scroll);
            }
            Some(OverlaySurface::Auth) => {
                let dialog = self.auth_dialog.as_mut().expect("auth surface");
                if dialog.has_interactive_options() {
                    dialog.move_selection(direction);
                } else {
                    dialog.scroll_by(if next { 1 } else { -1 }, auth_max_scroll);
                }
            }
            Some(OverlaySurface::Approval) => self
                .approval_dialog
                .as_mut()
                .expect("approval surface")
                .move_selection(next),
            Some(OverlaySurface::Question) => self
                .question_dialog
                .as_mut()
                .expect("question surface")
                .move_option(next),
            Some(OverlaySurface::Resume) => self
                .resume_picker
                .as_mut()
                .expect("resume surface")
                .move_selection(direction),
            Some(OverlaySurface::Queue) => self
                .queue_picker
                .as_mut()
                .expect("queue surface")
                .move_selection(next),
            Some(OverlaySurface::Dashboard) => {
                self.dashboard
                    .as_mut()
                    .expect("dashboard surface")
                    .scroll_by(
                        if next { 1 } else { -1 },
                        self.layout.transcript.height as usize,
                    );
            }
            Some(OverlaySurface::Settings) => {
                let dialog = self.settings_dialog.as_mut().expect("settings surface");
                dialog.selected_row = dialog.selected_row.move_by(next);
            }
            Some(OverlaySurface::Export) => {
                let flow = self.export_flow.as_mut().expect("export surface");
                if flow.step == ExportFlowStep::SelectSession {
                    flow.picker.move_selection(direction);
                }
            }
            None => {}
        }
    }

    fn close_resume_picker(&mut self) {
        self.resume_picker = None;
        self.status_message = "resume cancelled".to_owned();
    }

    fn open_auth_dialog(&mut self) {
        self.resume_picker = None;
        self.queue_picker = None;
        self.editing_queued_turn = None;
        self.input.clear();
        self.auth_dialog = Some(AuthDialogState::new());
        self.status_message = "connect a provider".to_owned();
    }

    async fn execute_auth_command(
        &mut self,
        transport: &RuntimeTransport,
        command: SlashAuthCommand,
    ) -> miette::Result<()> {
        if transport.is_remote()
            && !matches!(
                &command,
                SlashAuthCommand::Status | SlashAuthCommand::Protocols
            )
        {
            self.push_system_message(
                "Auth",
                vec![
                    "remote TUI cannot write provider credentials".to_owned(),
                    "configure the app-server host, then use /auth status".to_owned(),
                ],
            );
            return Ok(());
        }
        match command {
            SlashAuthCommand::Setup => {
                self.open_auth_dialog();
            }
            SlashAuthCommand::Status => {
                self.refresh_provider_status_from_runtime(transport).await;
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

    async fn abort(&mut self, transport: &RuntimeTransport) -> miette::Result<CommandAck> {
        let ack = transport
            .send_command(session_command(
                self.session_id,
                SessionCommandKind::Abort,
                json!({}),
            ))
            .await
            .map_err(|error| miette::miette!("{error}"))?;
        self.status_message = ack
            .reason
            .clone()
            .unwrap_or_else(|| "abort accepted".to_owned());
        self.refresh(transport).await?;
        Ok(ack)
    }

    async fn send_control_command(
        &mut self,
        transport: &RuntimeTransport,
        kind: SessionCommandKind,
    ) -> miette::Result<CommandAck> {
        let ack = transport
            .send_command(session_command(self.session_id, kind, json!({})))
            .await
            .map_err(|error| miette::miette!("{error}"))?;
        self.status_message = compact_ack_reason(&ack.reason);
        self.refresh(transport).await?;
        Ok(ack)
    }

    async fn resolve_pending_approval(
        &mut self,
        transport: &RuntimeTransport,
        kind: SessionCommandKind,
    ) -> miette::Result<CommandAck> {
        let approval_id = self
            .projection
            .as_ref()
            .and_then(|projection| projection.pending_approval.clone());
        let payload = approval_id.map_or_else(
            || json!({}),
            |approval_id| {
                json!({
                    "approval_id": approval_id,
                    "scope": ApprovalScope::Once,
                })
            },
        );
        let ack = transport
            .send_command(session_command(self.session_id, kind, payload))
            .await
            .map_err(|error| miette::miette!("{error}"))?;
        self.status_message = compact_ack_reason(&ack.reason);
        self.refresh(transport).await?;
        Ok(ack)
    }

    async fn resolve_approval_choice(
        &mut self,
        transport: &RuntimeTransport,
        choice: ApprovalChoice,
    ) -> miette::Result<CommandAck> {
        let Some(dialog) = self.approval_dialog.as_ref() else {
            return self
                .resolve_pending_approval(transport, SessionCommandKind::Approve)
                .await;
        };
        let kind = if choice == ApprovalChoice::Deny {
            SessionCommandKind::Deny
        } else {
            SessionCommandKind::Approve
        };
        let mut payload = json!({
            "approval_id": dialog.request.approval_id,
            "scope": choice.scope(),
        });
        if choice == ApprovalChoice::ResourcePrefix {
            payload["resource_prefix"] = json!(dialog.resource_prefix);
        }
        let ack = transport
            .send_command(session_command(self.session_id, kind, payload))
            .await
            .map_err(|error| miette::miette!("{error}"))?;
        self.status_message = compact_ack_reason(&ack.reason);
        if ack.accepted {
            self.approval_dialog = None;
        }
        self.refresh(transport).await?;
        Ok(ack)
    }

    async fn resolve_question(
        &mut self,
        transport: &RuntimeTransport,
        resolution: UserQuestionResolution,
    ) -> miette::Result<CommandAck> {
        let ack = transport
            .send_command(session_command(
                self.session_id,
                SessionCommandKind::AnswerQuestion,
                json!({
                    "question_id": resolution.question_id,
                    "answers": resolution.answers,
                }),
            ))
            .await
            .map_err(|error| miette::miette!("{error}"))?;
        self.status_message = compact_ack_reason(&ack.reason);
        if ack.accepted {
            self.question_dialog = None;
        }
        self.refresh(transport).await?;
        Ok(ack)
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
            let ack = self.abort(transport).await?;
            self.last_control_ack = Some(ack.clone());
            if ack.accepted {
                self.status_message = "interrupt requested; press Ctrl+C again to quit".to_owned();
            } else {
                self.status_message = format!(
                    "{}; press Ctrl+C again to quit",
                    compact_ack_reason(&ack.reason)
                );
            }
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
        self.invalidate_transcript_layout();
        self.sync_transcript_row_count(self.transcript_scroll.row_count);
        self.clamp_transcript_scroll();
    }
}

fn main() -> miette::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| miette::miette!("initialize Tokio runtime: {error}"))?;
    let result = runtime.block_on(async_main());
    // Tokio's portable stdin uses a blocking reader. A bounded shutdown keeps
    // the NDJSON `close` request authoritative even if the parent retains the
    // write end of the stdin pipe.
    runtime.shutdown_timeout(Duration::from_millis(250));
    result
}

async fn async_main() -> miette::Result<()> {
    let args = Args::parse();
    match args.command.clone() {
        Some(TuiCommand::Remote(command)) => {
            if args.connect.is_some() || args.daemon {
                return Err(miette::miette!(
                    "remote cannot be combined with --connect or --daemon"
                ));
            }
            let cwd = resolve_tui_cwd(&args)?;
            let transport = RuntimeTransport::connect(command.url, &cwd)
                .await
                .map_err(|error| miette::miette!("{error}"))?;
            return run_interactive(&args, cwd, transport).await;
        }
        Some(TuiCommand::Inspect(command)) => {
            return driver::run_inspect_command(&args, command).await;
        }
        Some(TuiCommand::Driver(command)) => {
            return driver::run_driver_command(&args, command).await;
        }
        None => {}
    }
    let cwd = resolve_tui_cwd(&args)?;
    let transport = if let Some(base_url) = args.connect.clone() {
        RuntimeTransport::connect(base_url, &cwd).await
    } else if args.daemon {
        RuntimeTransport::local_daemon(&cwd).await
    } else {
        RuntimeTransport::for_cwd_with_options(
            &cwd,
            RuntimeExecutionOptions::with_network_access(args.yolo),
        )
        .await
    }
    .map_err(|error| miette::miette!("{error}"))?;
    run_interactive(&args, cwd, transport).await
}

fn resolve_tui_cwd(args: &Args) -> miette::Result<PathBuf> {
    args.cwd
        .clone()
        .map_or_else(std::env::current_dir, Ok)
        .map_err(|error| miette::miette!("{error}"))
}

async fn run_interactive(
    args: &Args,
    cwd: PathBuf,
    transport: RuntimeTransport,
) -> miette::Result<()> {
    let task_id = parse_task_id(args.task_id.as_deref())?;
    let (thread_id, session_id) = initial_session(args.session_id.as_deref(), &transport).await?;
    let provider_status = initial_provider_ui_status(&transport, session_id).await;
    let runtime_cwd = transport.cwd().unwrap_or(&cwd).to_path_buf();
    let auth_dialog = (!transport.is_remote()).then(initial_auth_dialog).flatten();
    let app = TuiApp::new(
        thread_id,
        session_id,
        task_id,
        args.debug,
        provider_status.message,
        auth_dialog,
    )
    .with_yolo(args.yolo)
    .with_footer_context(runtime_cwd, provider_status.model);
    // The connected runtime stays authoritative for remote provider settings.
    let mut app = app
        .with_transport_runtime_controls(&transport)
        .with_loaded_preferences();
    app.enable_inline_history();
    let (terminal_width, terminal_height) = crossterm::terminal::size()
        .map_err(|error| miette::miette!("read terminal size: {error}"))?;
    let viewport_height = inline_viewport_height(&app, terminal_width, terminal_height);
    let mut terminal = setup_terminal(viewport_height)?;
    let terminal_restore = TerminalRestoreCoordinator::new(false);
    let panic_restore = terminal_restore.clone();
    install_terminal_panic_hook(move || {
        let _ = panic_restore.restore(&mut io::stdout());
    });
    let unwind_restore = terminal_restore.clone();
    let mut terminal_restore_guard = TerminalRestoreGuard::new(move || {
        let _ = unwind_restore.restore(&mut io::stdout());
    });
    let result = run_app(&mut terminal, app, transport).await;
    let restore = terminal_restore.restore(terminal.backend_mut());
    if restore.is_ok() {
        terminal_restore_guard.disarm();
    }
    combine_run_and_restore(result, restore)
}

#[derive(Clone)]
struct TerminalRestoreCoordinator {
    restored: Arc<AtomicBool>,
    use_alternate_screen: bool,
}

impl TerminalRestoreCoordinator {
    fn new(use_alternate_screen: bool) -> Self {
        Self {
            restored: Arc::new(AtomicBool::new(false)),
            use_alternate_screen,
        }
    }

    fn restore(&self, output: &mut impl io::Write) -> miette::Result<()> {
        if self.restored.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let result = restore_terminal(output, self.use_alternate_screen);
        if result.is_err() {
            self.restored.store(false, Ordering::Release);
        }
        result
    }
}

fn install_terminal_panic_hook(restore: impl Fn() + Send + Sync + 'static) {
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        restore_before_panic_report(&restore, || previous_hook(panic_info));
    }));
}

fn restore_before_panic_report(restore: impl FnOnce(), report: impl FnOnce()) {
    restore();
    report();
}

struct TerminalRestoreGuard<F: FnOnce()> {
    restore: Option<F>,
}

impl<F: FnOnce()> TerminalRestoreGuard<F> {
    fn new(restore: F) -> Self {
        Self {
            restore: Some(restore),
        }
    }

    fn disarm(&mut self) {
        self.restore = None;
    }
}

impl<F: FnOnce()> Drop for TerminalRestoreGuard<F> {
    fn drop(&mut self) {
        if let Some(restore) = self.restore.take() {
            restore();
        }
    }
}

async fn run_app(
    terminal: &mut InteractiveTerminal,
    mut app: TuiApp,
    transport: RuntimeTransport,
) -> miette::Result<()> {
    let mut controller = TuiRuntimeController::attach(&mut app, transport).await?;
    let mut terminal_events = EventStream::new();
    let mut maintenance = tokio::time::interval(INTERACTIVE_MAINTENANCE_INTERVAL);
    maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    maintenance.tick().await;
    let mut activity_status = tokio::time::interval(ACTIVITY_STATUS_INTERVAL);
    activity_status.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    activity_status.tick().await;
    let mut frames = FrameScheduler::default();
    let mut inline_history = InlineHistoryState::new(app.session_id);

    draw_interactive_frame(terminal, &mut app, &mut inline_history)?;
    frames.mark_drawn_at(Instant::now());

    while !app.should_quit {
        let frame_deadline = frames
            .deadline()
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(86_400));
        tokio::select! {
            _ = tokio::time::sleep_until(frame_deadline.into()), if frames.deadline().is_some() => {
                draw_interactive_frame(terminal, &mut app, &mut inline_history)?;
                frames.mark_drawn_at(Instant::now());
            }
            runtime_event = controller.recv() => {
                controller.apply_received(&mut app, runtime_event).await?;
                frames.request_at(Instant::now());
            }
            terminal_event = terminal_events.next() => {
                let event = terminal_event
                    .ok_or_else(|| miette::miette!("terminal input stream closed"))?
                    .map_err(|error| miette::miette!("{error}"))?;
                match event {
                CrosstermEvent::Key(key) => {
                    let resume_generation = app.terminal_resume_generation;
                    handle_key(key, &mut app, controller.transport()).await?;
                    if app.terminal_input_stream_is_stale(resume_generation) {
                        // Crossterm's stdin reader may remain parked after an external full-screen
                        // process. Recreate it after restoring raw mode so keyboard and mouse input
                        // cannot silently stop while the composer still appears usable.
                        terminal_events = EventStream::new();
                        terminal.clear().map_err(|error| miette::miette!("{error}"))?;
                    }
                    if app.last_prompt_ack.as_ref().is_some_and(|ack| ack.accepted) {
                        controller.replay_from_cursor(&app).await?;
                        app.take_last_prompt_ack();
                    }
                }
                CrosstermEvent::Mouse(mouse) => {
                    if let Some(activation) = handle_mouse(mouse, &mut app) {
                        execute_mouse_activation(activation, &mut app, controller.transport()).await?;
                    }
                }
                CrosstermEvent::Paste(pasted) => {
                    handle_paste(&pasted, &mut app);
                }
                _ => {}
                }
                frames.request_at(Instant::now());
            }
            _ = maintenance.tick() => {
                let changed = controller.sync_interactive(&mut app).await?;
                if changed
                    || app.auth_operation.is_some()
                    || app.export_operation.is_some()
                {
                    frames.request_at(Instant::now());
                }
            }
            _ = activity_status.tick(), if has_active_task(&app) => {
                let now = Instant::now();
                if app.activity_refresh_due(now) {
                    app.refresh_activity_snapshot();
                    frames.request_at(now);
                }
            }
        }
    }

    Ok(())
}

fn draw_interactive_frame(
    terminal: &mut InteractiveTerminal,
    app: &mut TuiApp,
    inline_history: &mut InlineHistoryState,
) -> miette::Result<()> {
    inline_history
        .flush_interactive(terminal, app)
        .map_err(|error| miette::miette!("write terminal history: {error}"))?;
    terminal
        .draw(|frame| draw_ui(frame, app))
        .map_err(|error| miette::miette!("{error}"))?;
    Ok(())
}

const INTERACTIVE_MAINTENANCE_INTERVAL: Duration = Duration::from_millis(80);
const ACTIVITY_STATUS_INTERVAL: Duration = Duration::from_millis(250);

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
    if key.code == KeyCode::F(1)
        || (key.code == KeyCode::Char('?')
            && key.modifiers.is_empty()
            && plain_question_mark_opens_help(app))
    {
        if app.help_dialog.take().is_some() {
            app.status_message = "help closed".to_owned();
        } else {
            app.open_help(HelpTopic::Overview);
        }
        return Ok(());
    }
    match app.overlay_surface() {
        Some(OverlaySurface::Help) => {
            handle_help_dialog_key(key, app);
            return Ok(());
        }
        Some(OverlaySurface::Auth) => return handle_auth_dialog_key(key, app, transport).await,
        Some(OverlaySurface::Approval) => {
            return handle_approval_dialog_key(key, app, transport).await;
        }
        Some(OverlaySurface::Question) => {
            return handle_question_dialog_key(key, app, transport).await;
        }
        Some(OverlaySurface::Resume) => {
            return handle_resume_picker_key(key, app, transport).await;
        }
        Some(OverlaySurface::Queue) => {
            return handle_queue_picker_key(key, app, transport).await;
        }
        Some(OverlaySurface::Dashboard) => {
            handle_dashboard_key(key, app);
            return Ok(());
        }
        Some(OverlaySurface::Settings) => {
            handle_settings_dialog_key(key, app);
            return Ok(());
        }
        Some(OverlaySurface::Export) => return handle_export_key(key, app, transport).await,
        None => {}
    }
    if app.history_search.is_some() {
        handle_history_search_key(key, app);
        return Ok(());
    }
    if app.transcript_search.is_some() {
        handle_transcript_search_key(key, app);
        return Ok(());
    }
    if app.input.is_empty()
        && key.code == KeyCode::Char('f')
        && key.modifiers.contains(KeyModifiers::CONTROL)
    {
        app.open_transcript_search();
        return Ok(());
    }
    if key.code == KeyCode::Char('t') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.toggle_transcript_fullscreen();
        return Ok(());
    }
    if key.code == KeyCode::Char('r') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.open_history_search();
        return Ok(());
    }
    if key.code == KeyCode::Char('p') && key.modifiers.contains(KeyModifiers::ALT) {
        app.dashboard = Some(DashboardState::new(DashboardTab::Plan));
        app.status_message = "execution dashboard".to_owned();
        return Ok(());
    }
    if key.code == KeyCode::Char('e') && key.modifiers.contains(KeyModifiers::ALT) {
        app.edit_prompt_in_external_editor();
        return Ok(());
    }
    if key.code == KeyCode::Char('q') && key.modifiers.contains(KeyModifiers::ALT) {
        app.open_queue_picker();
        return Ok(());
    }
    if key.code == KeyCode::Char('a') && key.modifiers.contains(KeyModifiers::ALT) {
        app.select_next_attachment();
        return Ok(());
    }
    if key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::ALT) {
        app.toggle_prompt_stash();
        return Ok(());
    }
    if key.code == KeyCode::Char('r') && key.modifiers.contains(KeyModifiers::ALT) {
        app.toggle_raw_transcript();
        return Ok(());
    }
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::ALT) {
        app.copy_transcript();
        return Ok(());
    }
    if key.code == KeyCode::Char('o') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.toggle_transcript_details();
        return Ok(());
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
                let ack = app
                    .resolve_pending_approval(transport, SessionCommandKind::Approve)
                    .await?;
                app.last_control_ack = Some(ack);
                return Ok(());
            }
            KeyCode::Char('n') if key.modifiers.is_empty() => {
                let ack = app
                    .resolve_pending_approval(transport, SessionCommandKind::Deny)
                    .await?;
                app.last_control_ack = Some(ack);
                return Ok(());
            }
            _ => {}
        }
    }

    if app.composer_mode == ComposerMode::VimInsert
        && key.code == KeyCode::Esc
        && key.modifiers.is_empty()
    {
        app.composer_mode = ComposerMode::VimNormal;
        app.vim_pending_operator = None;
        app.status_message = "Vim normal mode".to_owned();
        return Ok(());
    }
    if app.composer_mode == ComposerMode::VimNormal {
        return handle_vim_normal_key(key, app, transport).await;
    }

    match key.code {
        KeyCode::Esc => {
            handle_composer_escape(app, transport).await?;
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
            app.refresh_mention_completion();
            app.prompt_history.reset_navigation();
        }
        KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.delete_to_line_end();
            app.reset_slash_selection();
            app.refresh_mention_completion();
            app.prompt_history.reset_navigation();
        }
        KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.delete_word_backward();
            app.reset_slash_selection();
            app.refresh_mention_completion();
            app.prompt_history.reset_navigation();
        }
        KeyCode::Char('z') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.undo();
            app.refresh_mention_completion();
        }
        KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.redo();
            app.refresh_mention_completion();
        }
        KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::ALT) => {
            app.input.move_word_left();
        }
        KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::ALT) => {
            app.input.move_word_right();
        }
        KeyCode::Tab => {
            if app.accept_mention_completion() {
                app.status_message = "reference completed".to_owned();
            } else if app.move_slash_selection(ResumeSelectionDirection::Next) {
                app.status_message = "slash command selected".to_owned();
            }
        }
        KeyCode::Up => {
            if app.move_mention_selection(false) {
                app.status_message = "reference selected".to_owned();
            } else if app.move_slash_selection(ResumeSelectionDirection::Previous) {
                app.status_message = "slash command selected".to_owned();
            } else if !app.input.move_line_up() {
                app.previous_prompt();
            }
        }
        KeyCode::Down => {
            if app.move_mention_selection(true) {
                app.status_message = "reference selected".to_owned();
            } else if app.move_slash_selection(ResumeSelectionDirection::Next) {
                app.status_message = "slash command selected".to_owned();
            } else if !app.input.move_line_down() {
                app.next_prompt();
            }
        }
        KeyCode::PageUp => {
            app.scroll_active_pane(TranscriptScrollAction::PageUp);
        }
        KeyCode::PageDown => {
            app.scroll_active_pane(TranscriptScrollAction::PageDown);
        }
        KeyCode::Home => {
            if app.input.is_empty() {
                app.scroll_active_pane(TranscriptScrollAction::Top);
            } else {
                app.input.move_to_start();
            }
        }
        KeyCode::End => {
            if app.input.is_empty() {
                app.scroll_active_pane(TranscriptScrollAction::Bottom);
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
        KeyCode::Enter
            if key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
        {
            app.input.insert_newline();
            app.reset_slash_selection();
            app.refresh_mention_completion();
            app.prompt_history.reset_navigation();
        }
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if app.editing_queued_turn.is_some() {
                app.status_message = "finish or cancel the queued prompt edit first".to_owned();
                return Ok(());
            }
            let input = app.input.trimmed();
            match parse_slash_input(&input) {
                SlashInput::Prompt(prompt) => app.send_steering_prompt(transport, prompt).await?,
                SlashInput::Empty => app.status_message = "prompt is empty".to_owned(),
                SlashInput::Command(_) | SlashInput::Error(_) => {
                    app.status_message =
                        "steering accepts a prompt, not a slash command".to_owned();
                }
            }
        }
        KeyCode::Enter => {
            if app.accept_mention_completion() {
                app.status_message = "reference completed".to_owned();
            } else if !app.accept_slash_candidate(transport).await? {
                app.send_prompt(transport).await?;
            }
        }
        KeyCode::Backspace => {
            app.input.delete_backward();
            app.reset_slash_selection();
            app.refresh_mention_completion();
            app.prompt_history.reset_navigation();
        }
        KeyCode::Delete => {
            if !app.input.is_empty() || !app.remove_selected_attachment() {
                app.input.delete_forward();
                app.reset_slash_selection();
                app.refresh_mention_completion();
                app.prompt_history.reset_navigation();
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
            app.input.insert_char(character);
            app.reset_slash_selection();
            app.refresh_mention_completion();
            app.prompt_history.reset_navigation();
        }
        _ => {}
    }
    Ok(())
}

async fn handle_composer_escape(
    app: &mut TuiApp,
    transport: &RuntimeTransport,
) -> miette::Result<()> {
    if app.editing_queued_turn.take().is_some() {
        app.input.reset();
        app.attachments.clear();
        app.selected_attachment = None;
        app.mention_completion = None;
        app.status_message = "queued prompt edit cancelled".to_owned();
    } else if has_active_task(app) {
        let ack = app.abort(transport).await?;
        app.last_control_ack = Some(ack.clone());
        app.status_message = if ack.accepted {
            "interrupt requested".to_owned()
        } else {
            compact_ack_reason(&ack.reason)
        };
    } else if !app.input.is_empty() {
        app.input.clear();
        app.reset_slash_selection();
        app.refresh_mention_completion();
        app.prompt_history.reset_navigation();
        app.status_message = "input cleared".to_owned();
    } else {
        app.status_message = "press Ctrl+C twice to quit".to_owned();
    }
    Ok(())
}

async fn handle_vim_normal_key(
    key: KeyEvent,
    app: &mut TuiApp,
    transport: &RuntimeTransport,
) -> miette::Result<()> {
    if let Some(operator) = app.vim_pending_operator.take() {
        match (operator, key.code) {
            ('d', KeyCode::Char('d')) => app.input.delete_current_line(),
            ('d', KeyCode::Char('w')) => app.input.delete_word_forward(),
            ('d', KeyCode::Char('b')) => app.input.delete_word_backward(),
            ('d', KeyCode::Char('$')) => app.input.delete_to_line_end(),
            ('g', KeyCode::Char('g')) => app.input.move_to_start(),
            ('r', KeyCode::Char(character)) if key.modifiers.is_empty() => {
                app.input.delete_forward();
                app.input.insert_char(character);
                app.input.move_left();
            }
            _ => {
                app.status_message = "Vim operator cancelled".to_owned();
                return Ok(());
            }
        }
        after_composer_edit(app);
        return Ok(());
    }

    match key.code {
        KeyCode::Esc => handle_composer_escape(app, transport).await?,
        KeyCode::Enter => {
            if app.accept_mention_completion() {
                app.status_message = "reference completed".to_owned();
            } else if !app.accept_slash_candidate(transport).await? {
                app.send_prompt(transport).await?;
            }
        }
        KeyCode::Char('i') if key.modifiers.is_empty() => {
            app.composer_mode = ComposerMode::VimInsert;
            app.status_message = "Vim insert mode".to_owned();
        }
        KeyCode::Char('a') if key.modifiers.is_empty() => {
            app.input.move_right();
            app.composer_mode = ComposerMode::VimInsert;
            app.status_message = "Vim insert mode".to_owned();
        }
        KeyCode::Char('I') if key.modifiers.contains(KeyModifiers::SHIFT) => {
            app.input.move_to_line_start();
            app.composer_mode = ComposerMode::VimInsert;
        }
        KeyCode::Char('A') if key.modifiers.contains(KeyModifiers::SHIFT) => {
            app.input.move_to_line_end();
            app.composer_mode = ComposerMode::VimInsert;
        }
        KeyCode::Char('o') if key.modifiers.is_empty() => {
            app.input.insert_line_below();
            app.composer_mode = ComposerMode::VimInsert;
            after_composer_edit(app);
        }
        KeyCode::Char('O') if key.modifiers.contains(KeyModifiers::SHIFT) => {
            app.input.insert_line_above();
            app.composer_mode = ComposerMode::VimInsert;
            after_composer_edit(app);
        }
        KeyCode::Left | KeyCode::Char('h') => app.input.move_left(),
        KeyCode::Right | KeyCode::Char('l') => app.input.move_right(),
        KeyCode::Up | KeyCode::Char('k') => {
            app.input.move_line_up();
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.input.move_line_down();
        }
        KeyCode::Char('w') => app.input.move_word_right(),
        KeyCode::Char('b') => app.input.move_word_left(),
        KeyCode::Char('0') | KeyCode::Home => app.input.move_to_line_start(),
        KeyCode::Char('$') => app.input.move_to_line_end(),
        KeyCode::Char('G') if key.modifiers.contains(KeyModifiers::SHIFT) => {
            app.input.move_to_end();
        }
        KeyCode::Char('x') | KeyCode::Delete => {
            app.input.delete_forward();
            after_composer_edit(app);
        }
        KeyCode::Char('D') if key.modifiers.contains(KeyModifiers::SHIFT) => {
            app.input.delete_to_line_end();
            after_composer_edit(app);
        }
        KeyCode::Char('C') if key.modifiers.contains(KeyModifiers::SHIFT) => {
            app.input.delete_to_line_end();
            app.composer_mode = ComposerMode::VimInsert;
            after_composer_edit(app);
        }
        KeyCode::Char('s') if key.modifiers.is_empty() => {
            app.input.delete_forward();
            app.composer_mode = ComposerMode::VimInsert;
            after_composer_edit(app);
        }
        KeyCode::Char('d') | KeyCode::Char('g') | KeyCode::Char('r')
            if key.modifiers.is_empty() =>
        {
            app.vim_pending_operator = match key.code {
                KeyCode::Char(operator) => Some(operator),
                _ => None,
            };
            app.status_message = "Vim operator pending".to_owned();
        }
        KeyCode::Char('u') if key.modifiers.is_empty() => {
            app.input.undo();
            after_composer_edit(app);
        }
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.redo();
            after_composer_edit(app);
        }
        KeyCode::PageUp => app.scroll_active_pane(TranscriptScrollAction::PageUp),
        KeyCode::PageDown => app.scroll_active_pane(TranscriptScrollAction::PageDown),
        KeyCode::End => app.scroll_active_pane(TranscriptScrollAction::Bottom),
        _ => {}
    }
    Ok(())
}

fn after_composer_edit(app: &mut TuiApp) {
    app.reset_slash_selection();
    app.refresh_mention_completion();
    app.prompt_history.reset_navigation();
}

fn handle_history_search_key(key: KeyEvent, app: &mut TuiApp) {
    match key.code {
        KeyCode::Esc => {
            app.history_search = None;
            app.status_message = "prompt history search closed".to_owned();
            return;
        }
        KeyCode::Enter => {
            if let Some(prompt) = app
                .history_search
                .as_ref()
                .and_then(HistorySearchState::selected)
                .map(str::to_owned)
            {
                app.input.set_text(prompt);
            }
            app.history_search = None;
            app.refresh_mention_completion();
            app.status_message = "history prompt restored".to_owned();
            return;
        }
        KeyCode::Up => {
            if let Some(search) = &mut app.history_search {
                search.move_selection(false);
            }
            return;
        }
        KeyCode::Down => {
            if let Some(search) = &mut app.history_search {
                search.move_selection(true);
            }
            return;
        }
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(search) = &mut app.history_search {
                search.move_selection(true);
            }
            return;
        }
        _ => {}
    }

    let Some(search) = &mut app.history_search else {
        return;
    };
    match key.code {
        KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            search.input.move_to_start();
        }
        KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            search.input.move_to_end();
        }
        KeyCode::Left => search.input.move_left(),
        KeyCode::Right => search.input.move_right(),
        KeyCode::Backspace => search.input.delete_backward(),
        KeyCode::Delete => search.input.delete_forward(),
        KeyCode::Char(character)
            if !key.modifiers.intersects(
                KeyModifiers::CONTROL
                    | KeyModifiers::ALT
                    | KeyModifiers::SUPER
                    | KeyModifiers::HYPER
                    | KeyModifiers::META,
            ) =>
        {
            search.input.insert_char(character);
        }
        _ => return,
    }
    search.rebuild(&app.prompt_history);
}

fn handle_transcript_search_key(key: KeyEvent, app: &mut TuiApp) {
    match key.code {
        KeyCode::Esc => {
            app.close_transcript_search();
            return;
        }
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
            if let Some(search) = &mut app.transcript_search {
                search.select_previous();
            }
            app.focus_current_search_match();
            return;
        }
        KeyCode::Enter | KeyCode::Down => {
            if let Some(search) = &mut app.transcript_search {
                search.select_next();
            }
            app.focus_current_search_match();
            return;
        }
        KeyCode::Up => {
            if let Some(search) = &mut app.transcript_search {
                search.select_previous();
            }
            app.focus_current_search_match();
            return;
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::ALT) => {
            app.copy_transcript();
            return;
        }
        _ => {}
    }

    let Some(search) = &mut app.transcript_search else {
        return;
    };
    match key.code {
        KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            search.input.move_to_start();
        }
        KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            search.input.move_to_end();
        }
        KeyCode::Left => search.input.move_left(),
        KeyCode::Right => search.input.move_right(),
        KeyCode::Backspace => search.input.delete_backward(),
        KeyCode::Delete => search.input.delete_forward(),
        KeyCode::Char(character)
            if !key.modifiers.intersects(
                KeyModifiers::CONTROL
                    | KeyModifiers::ALT
                    | KeyModifiers::SUPER
                    | KeyModifiers::HYPER
                    | KeyModifiers::META,
            ) =>
        {
            search.input.insert_char(character);
        }
        _ => return,
    }
    app.rebuild_transcript_search();
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
            KeyCode::PageUp => {
                let page_size = resume_picker_page_size(app.layout.transcript);
                if let Some(flow) = &mut app.export_flow {
                    flow.picker
                        .move_selection_by_page(ResumeSelectionDirection::Previous, page_size);
                }
            }
            KeyCode::PageDown => {
                let page_size = resume_picker_page_size(app.layout.transcript);
                if let Some(flow) = &mut app.export_flow {
                    flow.picker
                        .move_selection_by_page(ResumeSelectionDirection::Next, page_size);
                }
            }
            KeyCode::Home => {
                if let Some(flow) = &mut app.export_flow {
                    flow.picker.select_first();
                }
            }
            KeyCode::End => {
                if let Some(flow) = &mut app.export_flow {
                    flow.picker.select_last();
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
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(input) = app.export_input_mut() {
                    input.move_to_start();
                }
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(input) = app.export_input_mut() {
                    input.move_to_end();
                }
            }
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(input) = app.export_input_mut() {
                    input.move_left();
                }
            }
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(input) = app.export_input_mut() {
                    input.move_right();
                }
            }
            KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(input) = app.export_input_mut() {
                    input.delete_backward();
                }
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(input) = app.export_input_mut() {
                    input.clear();
                }
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(input) = app.export_input_mut() {
                    input.delete_to_line_end();
                }
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(input) = app.export_input_mut() {
                    input.delete_word_backward();
                }
            }
            KeyCode::Char('z') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(input) = app.export_input_mut() {
                    input.undo();
                }
            }
            KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(input) = app.export_input_mut() {
                    input.redo();
                }
            }
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::ALT) => {
                if let Some(input) = app.export_input_mut() {
                    input.move_word_left();
                }
            }
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::ALT) => {
                if let Some(input) = app.export_input_mut() {
                    input.move_word_right();
                }
            }
            KeyCode::Left => {
                if let Some(input) = app.export_input_mut() {
                    input.move_left();
                }
            }
            KeyCode::Right => {
                if let Some(input) = app.export_input_mut() {
                    input.move_right();
                }
            }
            KeyCode::Home => {
                if let Some(input) = app.export_input_mut() {
                    input.move_to_start();
                }
            }
            KeyCode::End => {
                if let Some(input) = app.export_input_mut() {
                    input.move_to_end();
                }
            }
            KeyCode::Backspace => {
                if let Some(input) = app.export_input_mut() {
                    input.delete_backward();
                }
            }
            KeyCode::Delete => {
                if let Some(input) = app.export_input_mut() {
                    input.delete_forward();
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
                    input.insert_char(character);
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
    let normalized = pasted.replace("\r\n", "\n").replace('\r', "\n");
    let single_line = normalized.lines().collect::<Vec<_>>().join(" ");

    match app.overlay_surface() {
        Some(OverlaySurface::Help)
        | Some(OverlaySurface::Approval)
        | Some(OverlaySurface::Queue)
        | Some(OverlaySurface::Dashboard) => return,
        Some(OverlaySurface::Auth) => {
            let dialog = app.auth_dialog.as_mut().expect("auth surface");
            if let Some(input) = dialog.current_input_mut() {
                input.push_str(&normalized.replace('\n', ""));
                dialog.error = None;
            }
            return;
        }
        Some(OverlaySurface::Question) => {
            let dialog = app.question_dialog.as_mut().expect("question surface");
            dialog.focus_free_text(dialog.question_index);
            dialog.current_free_text_mut().insert_str(&normalized);
            return;
        }
        Some(OverlaySurface::Resume) => {
            let picker = app.resume_picker.as_mut().expect("resume surface");
            if picker.action == Some(SessionPickerAction::Rename) {
                picker.action_input.insert_str(&single_line);
            } else if picker.action.is_none() {
                picker.search.insert_str(&single_line);
                picker.refresh_search();
            }
            return;
        }
        Some(OverlaySurface::Settings) => {
            let dialog = app.settings_dialog.as_mut().expect("settings surface");
            if dialog.editing_model {
                dialog.model_input.insert_str(&single_line);
            }
            return;
        }
        Some(OverlaySurface::Export) => {
            if let Some(input) = app.export_input_mut() {
                input.insert_str(&single_line);
            }
            return;
        }
        None => {}
    }

    if let Some(search) = &mut app.history_search {
        search.input.insert_str(&single_line);
        search.rebuild(&app.prompt_history);
        return;
    }

    if app.transcript_search.is_some() {
        if let Some(search) = &mut app.transcript_search {
            search.input.insert_str(&single_line);
        }
        app.rebuild_transcript_search();
        return;
    }

    if app.composer_mode == ComposerMode::VimNormal {
        app.status_message = "enter Vim insert mode before pasting".to_owned();
        return;
    }

    app.input.insert_str(&normalized);
    app.reset_slash_selection();
    app.refresh_mention_completion();
    app.prompt_history.reset_navigation();
}

fn plain_question_mark_opens_help(app: &TuiApp) -> bool {
    app.help_dialog.is_some()
        || (app.input.is_empty()
            && app.overlay_surface().is_none()
            && app.history_search.is_none()
            && app.transcript_search.is_none())
}

fn handle_help_dialog_key(key: KeyEvent, app: &mut TuiApp) {
    let area = app.layout.transcript;
    let max_scroll = app
        .help_dialog
        .as_ref()
        .map_or(0, |dialog| help_scroll_max(dialog, app, area));
    let Some(dialog) = &mut app.help_dialog else {
        return;
    };
    let page = usize::from(area.height.saturating_sub(1)).max(1);
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.help_dialog = None;
            app.status_message = "help closed".to_owned();
        }
        KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => dialog.cycle(true),
        KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => dialog.cycle(false),
        KeyCode::Char('1') => dialog.set_topic(HelpTopic::Overview),
        KeyCode::Char('2') => dialog.set_topic(HelpTopic::Composer),
        KeyCode::Char('3') => dialog.set_topic(HelpTopic::Navigation),
        KeyCode::Char('4') => dialog.set_topic(HelpTopic::Runtime),
        KeyCode::Char('5') => {
            dialog.set_topic(HelpTopic::WhatsNew);
            app.preferences.mark_current_release_seen();
            app.release_badge_visible = false;
            app.persist_preferences();
        }
        KeyCode::Up | KeyCode::Char('k') => dialog.scroll_by(-1, max_scroll),
        KeyCode::Down | KeyCode::Char('j') => dialog.scroll_by(1, max_scroll),
        KeyCode::PageUp => dialog.scroll_by(-(page as isize), max_scroll),
        KeyCode::PageDown => dialog.scroll_by(page as isize, max_scroll),
        KeyCode::Home => dialog.scroll = 0,
        KeyCode::End => dialog.scroll = max_scroll,
        _ => {}
    }
}

fn handle_settings_dialog_key(key: KeyEvent, app: &mut TuiApp) {
    let Some(dialog) = &mut app.settings_dialog else {
        return;
    };
    if dialog.editing_model {
        match key.code {
            KeyCode::Esc => {
                dialog.editing_model = false;
                dialog.model_input.set_text(dialog.draft.effective_model());
                app.status_message = "model edit cancelled".to_owned();
            }
            KeyCode::Enter => match dialog.apply_model_input() {
                Ok(()) => app.status_message = "model override staged".to_owned(),
                Err(error) => app.status_message = error,
            },
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                dialog.model_input.move_to_start();
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                dialog.model_input.move_to_end();
            }
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                dialog.model_input.move_left();
            }
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                dialog.model_input.move_right();
            }
            KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                dialog.model_input.delete_backward();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                dialog.model_input.clear();
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                dialog.model_input.delete_to_line_end();
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                dialog.model_input.delete_word_backward();
            }
            KeyCode::Char('z') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                dialog.model_input.undo();
            }
            KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                dialog.model_input.redo();
            }
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::ALT) => {
                dialog.model_input.move_word_left();
            }
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::ALT) => {
                dialog.model_input.move_word_right();
            }
            KeyCode::Left => dialog.model_input.move_left(),
            KeyCode::Right => dialog.model_input.move_right(),
            KeyCode::Home => dialog.model_input.move_to_start(),
            KeyCode::End => dialog.model_input.move_to_end(),
            KeyCode::Backspace => dialog.model_input.delete_backward(),
            KeyCode::Delete => dialog.model_input.delete_forward(),
            KeyCode::Char(character)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL
                        | KeyModifiers::ALT
                        | KeyModifiers::SUPER
                        | KeyModifiers::HYPER
                        | KeyModifiers::META,
                ) =>
            {
                dialog.model_input.insert_char(character);
            }
            _ => {}
        }
        return;
    }

    match key.code {
        KeyCode::Esc => {
            app.settings_dialog = None;
            app.status_message = "session settings discarded".to_owned();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            dialog.selected_row = dialog.selected_row.move_by(false);
            dialog.unrestricted_confirmation = false;
        }
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
            dialog.selected_row = dialog.selected_row.move_by(true);
            dialog.unrestricted_confirmation = false;
        }
        KeyCode::Left | KeyCode::Char('h') => {
            if !dialog.cycle_selected(false) {
                app.status_message =
                    "runtime controls are locked while the task is active".to_owned();
            }
        }
        KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => {
            if !dialog.cycle_selected(true) {
                app.status_message =
                    "runtime controls are locked while the task is active".to_owned();
            }
        }
        KeyCode::Char('e')
            if dialog.selected_row == SettingsRow::Model && !dialog.runtime_locked =>
        {
            dialog.editing_model = true;
        }
        KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.apply_settings_dialog();
        }
        _ => {}
    }
}

async fn handle_approval_dialog_key(
    key: KeyEvent,
    app: &mut TuiApp,
    transport: &RuntimeTransport,
) -> miette::Result<()> {
    let mut submit = None;
    if let Some(dialog) = &mut app.approval_dialog {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => dialog.move_selection(false),
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => dialog.move_selection(true),
            KeyCode::Char('1') | KeyCode::Char('y') => submit = Some(ApprovalChoice::Once),
            KeyCode::Char('2') | KeyCode::Char('p') => {
                submit = Some(ApprovalChoice::ResourcePrefix);
            }
            KeyCode::Char('3') | KeyCode::Char('a') => submit = Some(ApprovalChoice::Session),
            KeyCode::Char('4') | KeyCode::Char('n') => submit = Some(ApprovalChoice::Deny),
            KeyCode::Enter => submit = Some(dialog.selected_choice()),
            KeyCode::Esc => {
                dialog.select(ApprovalChoice::Deny);
                app.status_message =
                    "approval remains pending; select Deny to reject it".to_owned();
            }
            _ => {}
        }
    }
    if let Some(choice) = submit {
        let ack = app.resolve_approval_choice(transport, choice).await?;
        app.last_control_ack = Some(ack);
    }
    Ok(())
}

async fn handle_question_dialog_key(
    key: KeyEvent,
    app: &mut TuiApp,
    transport: &RuntimeTransport,
) -> miette::Result<()> {
    let mut submit = false;
    let mut advance = false;
    if let Some(dialog) = &mut app.question_dialog {
        if key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL) {
            submit = dialog.all_answered();
            if !submit {
                app.status_message = "answer every question before submitting".to_owned();
            }
        } else if dialog.is_free_text_focused() {
            match key.code {
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Esc => dialog.toggle_focus(),
                KeyCode::Enter
                    if key
                        .modifiers
                        .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
                {
                    dialog.current_free_text_mut().insert_newline();
                }
                KeyCode::Enter => advance = true,
                KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    dialog.current_free_text_mut().move_to_start();
                }
                KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    dialog.current_free_text_mut().move_to_end();
                }
                KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    dialog.current_free_text_mut().move_left();
                }
                KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    dialog.current_free_text_mut().move_right();
                }
                KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    dialog.current_free_text_mut().delete_backward();
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    dialog.current_free_text_mut().clear();
                }
                KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    dialog.current_free_text_mut().delete_to_line_end();
                }
                KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    dialog.current_free_text_mut().delete_word_backward();
                }
                KeyCode::Char('z') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    dialog.current_free_text_mut().undo();
                }
                KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    dialog.current_free_text_mut().redo();
                }
                KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::ALT) => {
                    dialog.current_free_text_mut().move_word_left();
                }
                KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::ALT) => {
                    dialog.current_free_text_mut().move_word_right();
                }
                KeyCode::Up => {
                    dialog.current_free_text_mut().move_line_up();
                }
                KeyCode::Down => {
                    dialog.current_free_text_mut().move_line_down();
                }
                KeyCode::Left => dialog.current_free_text_mut().move_left(),
                KeyCode::Right => dialog.current_free_text_mut().move_right(),
                KeyCode::Home => dialog.current_free_text_mut().move_to_line_start(),
                KeyCode::End => dialog.current_free_text_mut().move_to_line_end(),
                KeyCode::Backspace => dialog.current_free_text_mut().delete_backward(),
                KeyCode::Delete => dialog.current_free_text_mut().delete_forward(),
                KeyCode::Char(character)
                    if !key.modifiers.intersects(
                        KeyModifiers::CONTROL
                            | KeyModifiers::ALT
                            | KeyModifiers::SUPER
                            | KeyModifiers::HYPER
                            | KeyModifiers::META,
                    ) =>
                {
                    dialog.current_free_text_mut().insert_char(character);
                }
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => dialog.move_option(false),
                KeyCode::Down | KeyCode::Char('j') => dialog.move_option(true),
                KeyCode::Left => dialog.move_question(false),
                KeyCode::Right => dialog.move_question(true),
                KeyCode::Tab | KeyCode::BackTab => dialog.toggle_focus(),
                KeyCode::Char(' ') => dialog.toggle_current(),
                KeyCode::Enter => {
                    if dialog.current_question().mode == golutra_core::UserQuestionMode::Single {
                        dialog.toggle_current();
                    }
                    advance = true;
                }
                KeyCode::Esc => {
                    app.status_message =
                        "the task is waiting for these answers; make a selection to continue"
                            .to_owned();
                }
                KeyCode::Backspace => {
                    dialog.toggle_focus();
                    dialog.current_free_text_mut().delete_backward();
                }
                KeyCode::Delete => {
                    dialog.toggle_focus();
                    dialog.current_free_text_mut().delete_forward();
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
                    dialog.toggle_focus();
                    dialog.current_free_text_mut().insert_char(character);
                }
                _ => {}
            }
        }
        if advance {
            if !dialog.current_answered() {
                app.status_message = "select an option or enter an answer".to_owned();
            } else if dialog.question_index + 1 < dialog.request.questions.len() {
                dialog.move_question(true);
            } else {
                submit = dialog.all_answered();
                if !submit {
                    app.status_message = "answer every question before submitting".to_owned();
                }
            }
        }
    }
    if submit
        && let Some(resolution) = app
            .question_dialog
            .as_ref()
            .and_then(|dialog| dialog.resolution("answered in TUI"))
    {
        let ack = app.resolve_question(transport, resolution).await?;
        app.last_control_ack = Some(ack);
    }
    Ok(())
}

fn handle_dashboard_key(key: KeyEvent, app: &mut TuiApp) {
    let Some(dashboard) = &mut app.dashboard else {
        return;
    };
    let page = usize::from(app.layout.transcript.height.saturating_sub(2)).max(1);
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.dashboard = None;
            app.status_message = "dashboard closed".to_owned();
        }
        KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => dashboard.cycle(true),
        KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => dashboard.cycle(false),
        KeyCode::Char('1') => dashboard.set_tab(DashboardTab::Plan),
        KeyCode::Char('2') => dashboard.set_tab(DashboardTab::Tasks),
        KeyCode::Char('3') => dashboard.set_tab(DashboardTab::Usage),
        KeyCode::Up | KeyCode::Char('k') => dashboard.scroll_by(-1, page),
        KeyCode::Down | KeyCode::Char('j') => dashboard.scroll_by(1, page),
        KeyCode::PageUp => dashboard.scroll_by(-(page as isize), page),
        KeyCode::PageDown => dashboard.scroll_by(page as isize, page),
        KeyCode::Home => dashboard.scroll = 0,
        _ => {}
    }
}

fn handle_mouse(mouse: MouseEvent, app: &mut TuiApp) -> Option<UiMouseActivation> {
    let target = app.layout.hit_test(mouse.column, mouse.row, app);
    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        if let Some(press) = mouse_press_at(app, mouse.column, mouse.row) {
            apply_mouse_press(app, press);
            app.mouse_press = Some(press);
            return None;
        }
        app.mouse_press = None;
        if target == UiHitTarget::Transcript
            && let Some(operation_id) =
                transcript_toggle_at(app, app.layout.transcript, mouse.column, mouse.row)
        {
            app.toggle_operation(operation_id);
            return None;
        }
    }
    match mouse.kind {
        MouseEventKind::Up(MouseButton::Left) => {
            let pressed = app.mouse_press.take();
            let released = mouse_press_at(app, mouse.column, mouse.row);
            if let Some(pressed) = pressed.filter(|press| Some(*press) == released) {
                match pressed {
                    UiMousePress::Approval(choice) => {
                        return Some(UiMouseActivation::Approval(choice));
                    }
                    UiMousePress::Auth(_) => {
                        return Some(UiMouseActivation::AuthContinue);
                    }
                    UiMousePress::Resume(_) => {
                        if app.resume_picker.is_some() {
                            return Some(UiMouseActivation::ResumeSession);
                        }
                    }
                    UiMousePress::QuestionOption { question, option } => {
                        if let Some(dialog) = &mut app.question_dialog
                            && dialog.focus(question, option)
                        {
                            dialog.toggle_current();
                        }
                    }
                    UiMousePress::QuestionFreeText { question } => {
                        if let Some(dialog) = &mut app.question_dialog {
                            dialog.focus_free_text(question);
                        }
                    }
                    UiMousePress::QuestionSubmit => {
                        if app
                            .question_dialog
                            .as_ref()
                            .is_some_and(QuestionDialogState::all_answered)
                        {
                            return Some(UiMouseActivation::QuestionSubmit);
                        }
                    }
                    UiMousePress::Settings(row) => {
                        if let Some(dialog) = &mut app.settings_dialog {
                            dialog.selected_row = row;
                            if !dialog.cycle_selected(true) {
                                app.status_message =
                                    "runtime controls are locked while the task is active"
                                        .to_owned();
                            }
                        }
                    }
                    UiMousePress::Queue(_) | UiMousePress::Dashboard(_) | UiMousePress::Help(_) => {
                    }
                }
            }
        }
        MouseEventKind::ScrollUp => {
            app.mouse_press = None;
            match target {
                UiHitTarget::Developer | UiHitTarget::Transcript => {
                    app.scroll_active_pane(TranscriptScrollAction::LineUp);
                }
                UiHitTarget::Overlay => {
                    app.move_overlay_selection(ResumeSelectionDirection::Previous);
                }
                UiHitTarget::Bottom | UiHitTarget::None => {}
            }
        }
        MouseEventKind::ScrollDown => {
            app.mouse_press = None;
            match target {
                UiHitTarget::Developer | UiHitTarget::Transcript => {
                    app.scroll_active_pane(TranscriptScrollAction::LineDown);
                }
                UiHitTarget::Overlay => {
                    app.move_overlay_selection(ResumeSelectionDirection::Next);
                }
                UiHitTarget::Bottom | UiHitTarget::None => {}
            }
        }
        _ => {}
    }
    None
}

fn mouse_press_at(app: &TuiApp, x: u16, y: u16) -> Option<UiMousePress> {
    overlay_mouse_press_at(app.layout.transcript, app, x, y)
}

fn apply_mouse_press(app: &mut TuiApp, press: UiMousePress) {
    match press {
        UiMousePress::Auth(index) => {
            if let Some(dialog) = &mut app.auth_dialog {
                dialog.set_interactive_selection(index);
            }
        }
        UiMousePress::Resume(index) => {
            if let Some(picker) = &mut app.resume_picker {
                picker.selected = index.min(picker.items.len().saturating_sub(1));
            } else if let Some(flow) = &mut app.export_flow {
                flow.picker.selected = index.min(flow.picker.items.len().saturating_sub(1));
            }
        }
        UiMousePress::Queue(index) => {
            if let Some(picker) = &mut app.queue_picker {
                picker.selected = index.min(picker.items.len().saturating_sub(1));
            }
        }
        UiMousePress::Approval(choice) => {
            if let Some(dialog) = &mut app.approval_dialog {
                dialog.select(choice);
            }
        }
        UiMousePress::QuestionOption { question, option } => {
            if let Some(dialog) = &mut app.question_dialog {
                dialog.focus(question, option);
            }
        }
        UiMousePress::QuestionFreeText { question } => {
            if let Some(dialog) = &mut app.question_dialog {
                dialog.focus_free_text(question);
            }
        }
        UiMousePress::QuestionSubmit => {}
        UiMousePress::Dashboard(tab) => {
            if let Some(dashboard) = &mut app.dashboard {
                dashboard.set_tab(tab);
            }
        }
        UiMousePress::Settings(row) => {
            if let Some(dialog) = &mut app.settings_dialog {
                dialog.selected_row = row;
            }
        }
        UiMousePress::Help(topic) => {
            if let Some(dialog) = &mut app.help_dialog {
                dialog.set_topic(topic);
            }
            if topic == HelpTopic::WhatsNew {
                app.preferences.mark_current_release_seen();
                app.release_badge_visible = false;
                app.persist_preferences();
            }
        }
    }
}

async fn execute_mouse_activation(
    activation: UiMouseActivation,
    app: &mut TuiApp,
    transport: &RuntimeTransport,
) -> miette::Result<()> {
    let ack = match activation {
        UiMouseActivation::AuthContinue => {
            advance_auth_dialog(app, transport).await?;
            return Ok(());
        }
        UiMouseActivation::ResumeSession => {
            app.resume_selected_thread(transport).await?;
            return Ok(());
        }
        UiMouseActivation::Approval(choice) => {
            app.resolve_approval_choice(transport, choice).await?
        }
        UiMouseActivation::QuestionSubmit => {
            let Some(resolution) = app
                .question_dialog
                .as_ref()
                .and_then(|dialog| dialog.resolution("answered in TUI"))
            else {
                app.status_message = "answer every question before submitting".to_owned();
                return Ok(());
            };
            app.resolve_question(transport, resolution).await?
        }
    };
    app.last_control_ack = Some(ack);
    Ok(())
}

async fn handle_resume_picker_key(
    key: KeyEvent,
    app: &mut TuiApp,
    transport: &RuntimeTransport,
) -> miette::Result<()> {
    if let Some(action) = app.resume_picker.as_ref().and_then(|picker| picker.action) {
        match action {
            SessionPickerAction::Rename => match key.code {
                KeyCode::Esc => {
                    if let Some(picker) = &mut app.resume_picker {
                        picker.finish_action();
                    }
                }
                KeyCode::Enter => app.apply_session_picker_action(transport).await?,
                KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(picker) = &mut app.resume_picker {
                        picker.action_input.move_to_start();
                    }
                }
                KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(picker) = &mut app.resume_picker {
                        picker.action_input.move_to_end();
                    }
                }
                KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(picker) = &mut app.resume_picker {
                        picker.action_input.move_left();
                    }
                }
                KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(picker) = &mut app.resume_picker {
                        picker.action_input.move_right();
                    }
                }
                KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(picker) = &mut app.resume_picker {
                        picker.action_input.delete_backward();
                    }
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(picker) = &mut app.resume_picker {
                        picker.action_input.clear();
                    }
                }
                KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(picker) = &mut app.resume_picker {
                        picker.action_input.delete_to_line_end();
                    }
                }
                KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(picker) = &mut app.resume_picker {
                        picker.action_input.delete_word_backward();
                    }
                }
                KeyCode::Char('z') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(picker) = &mut app.resume_picker {
                        picker.action_input.undo();
                    }
                }
                KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(picker) = &mut app.resume_picker {
                        picker.action_input.redo();
                    }
                }
                KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::ALT) => {
                    if let Some(picker) = &mut app.resume_picker {
                        picker.action_input.move_word_left();
                    }
                }
                KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::ALT) => {
                    if let Some(picker) = &mut app.resume_picker {
                        picker.action_input.move_word_right();
                    }
                }
                KeyCode::Left => {
                    if let Some(picker) = &mut app.resume_picker {
                        picker.action_input.move_left();
                    }
                }
                KeyCode::Right => {
                    if let Some(picker) = &mut app.resume_picker {
                        picker.action_input.move_right();
                    }
                }
                KeyCode::Home => {
                    if let Some(picker) = &mut app.resume_picker {
                        picker.action_input.move_to_start();
                    }
                }
                KeyCode::End => {
                    if let Some(picker) = &mut app.resume_picker {
                        picker.action_input.move_to_end();
                    }
                }
                KeyCode::Backspace => {
                    if let Some(picker) = &mut app.resume_picker {
                        picker.action_input.delete_backward();
                    }
                }
                KeyCode::Delete => {
                    if let Some(picker) = &mut app.resume_picker {
                        picker.action_input.delete_forward();
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
                    if let Some(picker) = &mut app.resume_picker {
                        picker.action_input.insert_char(character);
                    }
                }
                _ => {}
            },
            SessionPickerAction::Archive | SessionPickerAction::Delete => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    app.apply_session_picker_action(transport).await?;
                }
                KeyCode::Char('n') | KeyCode::Esc => {
                    if let Some(picker) = &mut app.resume_picker {
                        picker.finish_action();
                    }
                }
                _ => {}
            },
        }
        return Ok(());
    }
    match key.code {
        KeyCode::Esc => app.close_resume_picker(),
        KeyCode::Up | KeyCode::Char('k') if key.code == KeyCode::Up || key.modifiers.is_empty() => {
            app.move_resume_selection(ResumeSelectionDirection::Previous);
        }
        KeyCode::Down | KeyCode::Char('j')
            if key.code == KeyCode::Down || key.modifiers.is_empty() =>
        {
            app.move_resume_selection(ResumeSelectionDirection::Next);
        }
        KeyCode::PageUp => {
            let page_size = resume_picker_page_size(app.layout.transcript);
            if let Some(picker) = &mut app.resume_picker {
                picker.move_selection_by_page(ResumeSelectionDirection::Previous, page_size);
            }
        }
        KeyCode::PageDown => {
            let page_size = resume_picker_page_size(app.layout.transcript);
            if let Some(picker) = &mut app.resume_picker {
                picker.move_selection_by_page(ResumeSelectionDirection::Next, page_size);
            }
        }
        KeyCode::Home => {
            if let Some(picker) = &mut app.resume_picker {
                picker.select_first();
            }
        }
        KeyCode::End => {
            if let Some(picker) = &mut app.resume_picker {
                picker.select_last();
            }
        }
        KeyCode::Enter => {
            app.resume_selected_thread(transport).await?;
        }
        KeyCode::Char('i') if key.modifiers.contains(KeyModifiers::ALT) => {
            if let Some(picker) = &mut app.resume_picker {
                picker.show_details = !picker.show_details;
            }
        }
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::ALT) => {
            if let Some(picker) = &mut app.resume_picker {
                picker.begin_action(SessionPickerAction::Rename);
            }
        }
        KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::ALT) => {
            if let Some(picker) = &mut app.resume_picker {
                picker.begin_action(SessionPickerAction::Archive);
            }
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::ALT) => {
            if let Some(picker) = &mut app.resume_picker {
                picker.begin_action(SessionPickerAction::Delete);
            }
        }
        KeyCode::Backspace => {
            if let Some(picker) = &mut app.resume_picker {
                picker.search.delete_backward();
                picker.refresh_search();
            }
        }
        KeyCode::Delete => {
            if let Some(picker) = &mut app.resume_picker {
                picker.search.delete_forward();
                picker.refresh_search();
            }
        }
        KeyCode::Left => {
            if let Some(picker) = &mut app.resume_picker {
                picker.search.move_left();
            }
        }
        KeyCode::Right => {
            if let Some(picker) = &mut app.resume_picker {
                picker.search.move_right();
            }
        }
        KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(picker) = &mut app.resume_picker {
                picker.search.move_left();
            }
        }
        KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(picker) = &mut app.resume_picker {
                picker.search.move_right();
            }
        }
        KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(picker) = &mut app.resume_picker {
                picker.search.move_to_start();
            }
        }
        KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(picker) = &mut app.resume_picker {
                picker.search.move_to_end();
            }
        }
        KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(picker) = &mut app.resume_picker {
                picker.search.delete_backward();
                picker.refresh_search();
            }
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(picker) = &mut app.resume_picker {
                picker.search.clear();
                picker.refresh_search();
            }
        }
        KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(picker) = &mut app.resume_picker {
                picker.search.delete_to_line_end();
                picker.refresh_search();
            }
        }
        KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(picker) = &mut app.resume_picker {
                picker.search.delete_word_backward();
                picker.refresh_search();
            }
        }
        KeyCode::Char('z') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(picker) = &mut app.resume_picker {
                picker.search.undo();
                picker.refresh_search();
            }
        }
        KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(picker) = &mut app.resume_picker {
                picker.search.redo();
                picker.refresh_search();
            }
        }
        KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::ALT) => {
            if let Some(picker) = &mut app.resume_picker {
                picker.search.move_word_left();
            }
        }
        KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::ALT) => {
            if let Some(picker) = &mut app.resume_picker {
                picker.search.move_word_right();
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
            if let Some(picker) = &mut app.resume_picker {
                picker.search.insert_char(character);
                picker.refresh_search();
            }
        }
        _ => {}
    }
    Ok(())
}

async fn handle_queue_picker_key(
    key: KeyEvent,
    app: &mut TuiApp,
    transport: &RuntimeTransport,
) -> miette::Result<()> {
    match key.code {
        KeyCode::Esc => {
            app.queue_picker = None;
            app.status_message = "queued prompt manager closed".to_owned();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if let Some(picker) = &mut app.queue_picker {
                picker.move_selection(false);
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Some(picker) = &mut app.queue_picker {
                picker.move_selection(true);
            }
        }
        KeyCode::Home => {
            if let Some(picker) = &mut app.queue_picker {
                picker.select_first();
            }
        }
        KeyCode::End => {
            if let Some(picker) = &mut app.queue_picker {
                picker.select_last();
            }
        }
        KeyCode::Enter | KeyCode::Char('e') => app.edit_selected_queued_prompt(),
        KeyCode::Delete | KeyCode::Backspace | KeyCode::Char('d') => {
            app.cancel_selected_queued_prompt(transport).await?;
        }
        _ => {}
    }
    Ok(())
}

type InteractiveTerminal = Terminal<CursorFallbackBackend<CrosstermBackend<Stdout>>>;

fn setup_terminal(viewport_height: u16) -> miette::Result<InteractiveTerminal> {
    enable_raw_mode().map_err(|error| miette::miette!("{error}"))?;
    let mut stdout = io::stdout();
    if let Err(error) = execute!(stdout, EnableBracketedPaste, SetCursorStyle::SteadyBar) {
        return Err(rollback_terminal_setup(error, false));
    }
    match Terminal::with_options(
        CursorFallbackBackend::new(CrosstermBackend::new(stdout)),
        TerminalOptions {
            viewport: Viewport::Inline(viewport_height.max(1)),
        },
    ) {
        Ok(terminal) => {
            set_alternate_screen_active(false);
            Ok(terminal)
        }
        Err(error) => Err(rollback_terminal_setup(error, false)),
    }
}

fn restore_terminal(output: &mut impl io::Write, use_alternate_screen: bool) -> miette::Result<()> {
    let mut failures = Vec::new();
    record_terminal_failure(&mut failures, "disable raw mode", disable_raw_mode());
    record_terminal_failure(
        &mut failures,
        "disable bracketed paste",
        execute!(output, DisableBracketedPaste),
    );
    record_terminal_failure(
        &mut failures,
        "disable mouse capture",
        execute!(output, event::DisableMouseCapture),
    );
    record_terminal_failure(
        &mut failures,
        "restore cursor style",
        execute!(output, SetCursorStyle::DefaultUserShape),
    );
    if use_alternate_screen {
        record_terminal_failure(
            &mut failures,
            "leave alternate screen",
            execute!(output, LeaveAlternateScreen),
        );
    }
    record_terminal_failure(
        &mut failures,
        "show cursor",
        execute!(output, crossterm::cursor::Show),
    );
    set_alternate_screen_active(false);
    if failures.is_empty() {
        Ok(())
    } else {
        Err(miette::miette!(failures.join("; ")))
    }
}

fn combine_run_and_restore(
    run: miette::Result<()>,
    restore: miette::Result<()>,
) -> miette::Result<()> {
    match (run, restore) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(run_error), Err(restore_error)) => Err(miette::miette!(
            "{run_error}; terminal restore failed: {restore_error}"
        )),
    }
}

fn rollback_terminal_setup(error: io::Error, alternate_screen_entered: bool) -> miette::Report {
    let mut failures = vec![format!("terminal setup failed: {error}")];
    let mut stdout = io::stdout();
    record_terminal_failure(
        &mut failures,
        "disable bracketed paste after setup failure",
        execute!(stdout, DisableBracketedPaste),
    );
    record_terminal_failure(
        &mut failures,
        "disable mouse capture after setup failure",
        execute!(stdout, event::DisableMouseCapture),
    );
    record_terminal_failure(
        &mut failures,
        "restore cursor after setup failure",
        execute!(stdout, SetCursorStyle::DefaultUserShape),
    );
    if alternate_screen_entered {
        record_terminal_failure(
            &mut failures,
            "leave alternate screen after setup failure",
            execute!(stdout, LeaveAlternateScreen),
        );
    }
    record_terminal_failure(
        &mut failures,
        "disable raw mode after setup failure",
        disable_raw_mode(),
    );
    set_alternate_screen_active(false);
    miette::miette!(failures.join("; "))
}

fn record_terminal_failure(
    failures: &mut Vec<String>,
    label: &'static str,
    result: io::Result<()>,
) {
    if let Err(error) = result {
        failures.push(format!("{label}: {error}"));
    }
}

#[cfg(test)]
mod tests;
