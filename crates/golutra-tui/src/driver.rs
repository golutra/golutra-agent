//! Agent-facing offscreen TUI driver.

use std::{
    collections::VecDeque,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use golutra_client::{RuntimeClient, RuntimeTransport};
use golutra_core::{ActorKind, CommandId, QueryId, RedactionStatus, TaskId, TaskStatus, TurnId};
use golutra_protocol::{
    CommandAck, DriverControllerMode, DriverKey, DriverMetrics, DriverNotification,
    DriverNotificationKind, DriverRequest, DriverResponse, DriverResponseEnvelope, DriverState,
    DriverTaskStatus, ReadyResponse, RowRange, RuntimeQuery, RuntimeQueryKind, SnapshotDetail,
    SnapshotPanes, SnapshotRequest, SnapshotScope, StateProjection,
    TUI_DRIVER_MIN_PROTOCOL_VERSION, TUI_DRIVER_PROTOCOL_VERSION, TuiFrame, WaitCondition,
    response,
};
use ratatui::{Terminal, backend::TestBackend};
use uuid::Uuid;

use super::*;

mod frame;
mod io;
mod metrics;
mod session;
mod wait;

use frame::*;
pub(crate) use io::{run_driver_command, run_inspect_command};
use metrics::DriverMetricsAccumulator;
use session::*;
use wait::*;

const MAX_DRIVER_LINE_BYTES: usize = 1024 * 1024;
const MAX_DRIVER_INPUT_BYTES: usize = 256 * 1024;
const MAX_WAIT_MILLIS: u64 = 10 * 60 * 1000;
const DEFAULT_WAIT_MILLIS: u64 = 120 * 1000;
const MAX_PENDING_WAITS: usize = 64;
const FRAME_CACHE_CAPACITY: usize = 8;
const FRAME_CACHE_TTL: Duration = Duration::from_secs(60);

struct CachedFrame {
    created_at: tokio::time::Instant,
    frame: TuiFrame,
}

struct TransportCleanupGuard {
    transport: Option<RuntimeTransport>,
}

impl TransportCleanupGuard {
    fn new(transport: RuntimeTransport) -> Self {
        Self {
            transport: Some(transport),
        }
    }

    async fn close(&mut self) {
        if let Some(transport) = self.transport.take() {
            let _ = transport.close().await;
        }
    }

    fn disarm(&mut self) {
        self.transport = None;
    }
}

impl Drop for TransportCleanupGuard {
    fn drop(&mut self) {
        let Some(transport) = self.transport.take() else {
            return;
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = transport.close().await;
            });
        }
    }
}

#[derive(Clone)]
struct SnapshotUiState {
    projection: Option<UserProjection>,
    command_messages: Vec<TranscriptItem>,
    resume_picker: Option<ResumePickerState>,
    queue_picker: Option<QueuePickerState>,
    approval_dialog: Option<ApprovalDialogState>,
    question_dialog: Option<QuestionDialogState>,
    settings_dialog: Option<SettingsDialogState>,
    export_flow: Option<ExportFlowState>,
    auth_dialog: Option<AuthDialogState>,
    input: ComposerInput,
    history_search: Option<HistorySearchState>,
    transcript_search: Option<TranscriptSearchState>,
    attachments: Vec<ComposerAttachment>,
    mention_completion: Option<MentionCompletion>,
    status_message: String,
    provider_message: String,
    developer_error: Option<String>,
    runtime_controls: RuntimeControls,
}

impl SnapshotUiState {
    fn capture(app: &TuiApp) -> Self {
        Self {
            projection: app.projection.clone(),
            command_messages: app.command_messages.clone(),
            resume_picker: app.resume_picker.clone(),
            queue_picker: app.queue_picker.clone(),
            approval_dialog: app.approval_dialog.clone(),
            question_dialog: app.question_dialog.clone(),
            settings_dialog: app.settings_dialog.clone(),
            export_flow: app.export_flow.clone(),
            auth_dialog: app.auth_dialog.clone(),
            input: app.input.clone(),
            history_search: app.history_search.clone(),
            transcript_search: app.transcript.search.clone(),
            attachments: app.attachments.clone(),
            mention_completion: app.mention_completion.clone(),
            status_message: app.status_message.clone(),
            provider_message: app.provider_message.clone(),
            developer_error: app.developer_error.clone(),
            runtime_controls: app.runtime_controls.clone(),
        }
    }

    fn restore(self, app: &mut TuiApp) {
        app.projection = self.projection;
        app.command_messages = self.command_messages;
        app.resume_picker = self.resume_picker;
        app.queue_picker = self.queue_picker;
        app.approval_dialog = self.approval_dialog;
        app.question_dialog = self.question_dialog;
        app.settings_dialog = self.settings_dialog;
        app.export_flow = self.export_flow;
        app.auth_dialog = self.auth_dialog;
        app.input = self.input;
        app.history_search = self.history_search;
        app.transcript.search = self.transcript_search;
        app.attachments = self.attachments;
        app.mention_completion = self.mention_completion;
        app.status_message = self.status_message;
        app.provider_message = self.provider_message;
        app.developer_error = self.developer_error;
        app.runtime_controls = self.runtime_controls;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WaitFactsCacheKey {
    event_count: usize,
    cursor: Option<u64>,
    status: Option<TaskStatus>,
    task_id: Option<TaskId>,
    projection_ready: bool,
}

struct CachedWaitFacts {
    key: WaitFactsCacheKey,
    facts: Arc<WaitFacts>,
}

#[derive(Debug, Clone, Copy)]
struct SubmissionAnchor {
    command_id: CommandId,
    after_sequence_no: Option<u64>,
    task_id: Option<TaskId>,
    turn_id: Option<TurnId>,
}

pub(crate) struct TuiDriver {
    instance_id: String,
    app: TuiApp,
    controller: TuiRuntimeController,
    width: u16,
    height: u16,
    frame_cache: VecDeque<CachedFrame>,
    closed: bool,
    last_notified_cursor: Option<u64>,
    last_notified_status: Option<TaskStatus>,
    submission: Option<SubmissionAnchor>,
    last_controller_mode: DriverControllerMode,
    wait_facts_cache: Option<CachedWaitFacts>,
    metrics: DriverMetricsAccumulator,
}

impl TuiDriver {
    async fn launch(
        transport: RuntimeTransport,
        session: Option<&str>,
        task_id: Option<&str>,
        debug: bool,
        yolo: bool,
        width: u16,
        height: u16,
    ) -> miette::Result<Self> {
        // The transport may already own a remote attachment. Establish its
        // cleanup owner before any validation or lookup can return early.
        let mut cleanup = TransportCleanupGuard::new(transport.clone());
        validate_dimensions(width, height)?;
        let (thread_id, session_id) = resolve_driver_session(session, &transport).await?;
        let task_id = parse_task_id(task_id)?;
        validate_task_id(task_id, session_id, &transport).await?;
        let provider_status = initial_provider_ui_status(&transport, session_id).await;
        let runtime_cwd = transport
            .cwd()
            .map(Path::to_path_buf)
            .ok_or_else(|| miette::miette!("TUI driver transport has no workspace"))?;
        let auth_dialog = if task_id.is_some() || transport.is_remote() {
            None
        } else {
            initial_auth_dialog()
        };
        let mut app = TuiApp::new(
            thread_id,
            session_id,
            task_id,
            debug,
            provider_status.message,
            auth_dialog,
        )
        .with_yolo(yolo)
        .with_footer_context(runtime_cwd, provider_status.model);
        let controller = match TuiRuntimeController::attach(&mut app, transport).await {
            Ok(controller) => controller,
            Err(error) => {
                cleanup.close().await;
                return Err(error);
            }
        };
        let last_notified_cursor = app.cursor;
        let last_notified_status = app.projection.as_ref().map(|projection| projection.status);
        let mut driver = Self {
            instance_id: Uuid::now_v7().to_string(),
            app,
            controller,
            width,
            height,
            frame_cache: VecDeque::new(),
            closed: false,
            last_notified_cursor,
            last_notified_status,
            submission: None,
            last_controller_mode: DriverControllerMode::Controller,
            wait_facts_cache: None,
            metrics: DriverMetricsAccumulator::default(),
        };
        if let Err(error) = driver.controller_mode().await {
            let _ = driver.controller.shutdown().await;
            cleanup.close().await;
            return Err(error);
        }
        if let Err(error) = driver.refresh_active_layout() {
            let _ = driver.controller.shutdown().await;
            cleanup.close().await;
            return Err(error);
        }
        cleanup.disarm();
        Ok(driver)
    }

    async fn ready(&mut self) -> miette::Result<ReadyResponse> {
        self.refresh_controller_mode().await;
        Ok(ReadyResponse {
            protocol_version: TUI_DRIVER_PROTOCOL_VERSION,
            minimum_protocol_version: TUI_DRIVER_MIN_PROTOCOL_VERSION,
            instance_id: self.instance_id.clone(),
            workspace_id: self.controller.transport().workspace_id().to_string(),
            workspace_path: self
                .controller
                .transport()
                .cwd()
                .map_or_else(String::new, |path| path.display().to_string()),
            thread_id: self.app.thread_id.to_string(),
            session_id: self.app.session_id.to_string(),
            controller_mode: self.last_controller_mode,
        })
    }

    pub(crate) async fn shutdown(&mut self) -> miette::Result<()> {
        self.controller.shutdown().await
    }

    async fn state(&mut self) -> miette::Result<DriverState> {
        self.refresh_controller_mode().await;
        Ok(self.cached_state())
    }

    fn cached_state(&self) -> DriverState {
        self.cached_state_for_scope(WaitResponseScope::Current)
    }

    fn cached_state_for_scope(&self, scope: WaitResponseScope) -> DriverState {
        let (task_id, turn_id) = task_and_turn_for_scope(&self.app, scope);
        let status = match scope {
            WaitResponseScope::Current => self
                .app
                .projection
                .as_ref()
                .map_or(DriverTaskStatus::Connecting, |projection| {
                    projection.status.into()
                }),
            WaitResponseScope::Submission { status, .. } => status
                .map(Into::into)
                .unwrap_or(DriverTaskStatus::Connecting),
        };
        DriverState {
            instance_id: self.instance_id.clone(),
            thread_id: self.app.thread_id.to_string(),
            session_id: self.app.session_id.to_string(),
            task_id: task_id.map(|id| id.to_string()),
            turn_id: turn_id.map(|id| id.to_string()),
            status,
            width: self.width,
            height: self.height,
            facts_expanded: self.app.debug_mode
                && self.app.body_view_mode != BodyViewMode::Transcript
                && self.app.developer_observations_expanded,
            controller_mode: self.last_controller_mode,
            closed: self.closed,
        }
    }

    fn wait_state(
        &mut self,
        condition: &WaitCondition,
        submission: Option<SubmissionAnchor>,
    ) -> DriverState {
        let scope = {
            let facts = self.wait_facts();
            facts.response_scope(condition, submission)
        };
        self.cached_state_for_scope(scope)
    }

    async fn controller_mode(&self) -> miette::Result<DriverControllerMode> {
        let value = self
            .controller
            .transport()
            .query(RuntimeQuery {
                query_id: QueryId::new(),
                session_id: self.app.session_id,
                task_id: None,
                kind: RuntimeQueryKind::SessionState,
                requester: ActorKind::Tui,
                cursor: None,
                timestamp: chrono::Utc::now(),
            })
            .await
            .map_err(|error| miette::miette!("{error}"))?;
        let projection: StateProjection =
            serde_json::from_value(value).map_err(|error| miette::miette!("{error}"))?;
        let control_actor_id = self
            .controller
            .transport()
            .control_actor_id(TUI_ACTOR_ID.as_str());
        Ok(projection
            .runtime_lane
            .map_or(DriverControllerMode::Controller, |lane| {
                if lane.active_controller.id == control_actor_id {
                    DriverControllerMode::Controller
                } else {
                    DriverControllerMode::Observer
                }
            }))
    }

    async fn refresh_controller_mode(&mut self) {
        if let Ok(Ok(mode)) =
            tokio::time::timeout(Duration::from_millis(250), self.controller_mode()).await
        {
            self.last_controller_mode = mode;
        }
    }

    async fn sync(&mut self) -> miette::Result<()> {
        let started_at = self.metrics.start_sync();
        let result = self.controller.sync(&mut self.app).await;
        self.metrics.finish_sync(started_at, result.is_ok());
        result.map(|_| ())
    }

    fn metrics(&mut self, pending_waits: usize) -> DriverMetrics {
        self.prune_frames();
        self.metrics
            .snapshot(&self.instance_id, pending_waits, self.frame_cache.len())
    }

    fn record_connection(&mut self) {
        self.metrics.record_connection();
    }

    fn record_rejected_connection(&mut self) {
        self.metrics.record_rejected_connection();
    }

    fn record_request(&mut self) {
        self.metrics.record_request();
    }

    fn record_request_error(&mut self) {
        self.metrics.record_request_error();
    }

    fn start_wait_metrics(&mut self) -> Instant {
        self.metrics.start_wait()
    }

    fn finish_wait_metrics(&mut self, started_at: Instant, timed_out: bool) {
        if timed_out {
            self.metrics.finish_wait_timeout(started_at);
        } else {
            self.metrics.finish_wait_result(started_at);
        }
    }

    fn cancel_wait_metrics(&mut self, started_at: Instant) {
        self.metrics.cancel_wait(started_at);
    }

    async fn handle(&mut self, request: DriverRequest) -> DriverResponse {
        match self.try_handle(request).await {
            Ok(response) => response,
            Err(error) => DriverResponse::Error {
                code: driver_error_code(&error),
                message: bounded_error(&error.to_string()),
            },
        }
    }

    async fn try_handle(&mut self, request: DriverRequest) -> miette::Result<DriverResponse> {
        if self.closed && !matches!(request, DriverRequest::State | DriverRequest::Close { .. }) {
            return Ok(DriverResponse::Error {
                code: "driver_closed".to_owned(),
                message: "the TUI driver is closed".to_owned(),
            });
        }
        match request {
            DriverRequest::Hello { protocol_version } => {
                if protocol_version.is_some_and(|version| {
                    !(TUI_DRIVER_MIN_PROTOCOL_VERSION..=TUI_DRIVER_PROTOCOL_VERSION)
                        .contains(&version)
                }) {
                    return Ok(DriverResponse::Error {
                        code: "protocol_version_mismatch".to_owned(),
                        message: format!(
                            "TUI driver protocol {TUI_DRIVER_PROTOCOL_VERSION} is required"
                        ),
                    });
                }
                Ok(DriverResponse::Ready {
                    ready: self.ready().await?,
                })
            }
            DriverRequest::Capabilities => Ok(DriverResponse::Capabilities {
                capabilities: capabilities(),
            }),
            DriverRequest::State => Ok(DriverResponse::State {
                state: self.state().await?,
            }),
            DriverRequest::Ping => Ok(DriverResponse::Pong),
            DriverRequest::Metrics => Ok(DriverResponse::Metrics {
                metrics: self.metrics(0),
            }),
            DriverRequest::InputPrompt { text } => {
                validate_input(&text)?;
                ensure_task_binding_accepts_no_prompt(self.app.task_id)?;
                let after_sequence_no = self.app.cursor;
                let ack = self
                    .app
                    .submit_runtime_prompt(self.controller.transport(), text)
                    .await?
                    .ok_or_else(|| miette::miette!("invalid_input: prompt is empty"))?;
                ensure_command_accepted(&ack, "runtime rejected the prompt")?;
                self.app.take_last_prompt_ack();
                self.controller.replay_from_cursor(&self.app).await?;
                self.sync().await?;
                self.record_submission(ack, after_sequence_no);
                self.refresh_active_layout()?;
                Ok(DriverResponse::Accepted {
                    message: "prompt submitted".to_owned(),
                })
            }
            DriverRequest::InputSlash { text } => {
                validate_input(&text)?;
                if !text.trim_start().starts_with('/') {
                    return Ok(DriverResponse::Error {
                        code: "invalid_slash_command".to_owned(),
                        message: "input_slash requires text beginning with a slash".to_owned(),
                    });
                }
                ensure_slash_input_is_valid(&text)?;
                ensure_session_binding_is_immutable(&text)?;
                ensure_task_binding_allows_slash(self.app.task_id, &text)?;
                self.app.take_last_control_ack();
                let SlashInput::Command(command) = parse_slash_input(&text) else {
                    unreachable!("validated slash input must be a command");
                };
                ensure_modal_allows_slash(&self.app, &command)?;
                self.app
                    .execute_slash_command(self.controller.transport(), command)
                    .await?;
                if self.app.should_quit {
                    self.closed = true;
                    return Ok(DriverResponse::Closed);
                }
                self.sync().await?;
                if let Some(ack) = self.app.take_last_control_ack() {
                    ensure_command_accepted(&ack, "runtime rejected slash command")?;
                }
                Ok(DriverResponse::Accepted {
                    message: "slash command handled".to_owned(),
                })
            }
            DriverRequest::InputKey { key } => {
                let after_sequence_no = self.app.cursor;
                self.app.take_last_prompt_ack();
                self.app.take_last_control_ack();
                if let DriverKey::Char(text) = &key {
                    validate_input(text)?;
                    ensure_driver_input_capacity(&self.app, text.len())?;
                }
                ensure_driver_binding_allows_key(self.app.task_id, &self.app, &key)?;
                if matches!(key, DriverKey::Enter) && driver_enter_reaches_composer(&self.app) {
                    let candidate = pending_slash_completion(&self.app);
                    if candidate
                        .as_ref()
                        .is_none_or(|candidate| candidate.execute_on_select)
                    {
                        let executable_input = candidate
                            .map(|candidate| candidate.command)
                            .unwrap_or_else(|| self.app.input.text().to_owned());
                        ensure_slash_input_is_valid(&executable_input)?;
                        ensure_session_binding_is_immutable(&executable_input)?;
                        match parse_slash_input(&executable_input) {
                            SlashInput::Prompt(_) => {
                                ensure_task_binding_accepts_no_prompt(self.app.task_id)?;
                            }
                            SlashInput::Command(_) => {
                                ensure_task_binding_allows_slash(
                                    self.app.task_id,
                                    &executable_input,
                                )?;
                            }
                            SlashInput::Empty => {}
                            SlashInput::Error(_) => unreachable!("validated above"),
                        }
                    }
                }
                self.handle_key_input(key).await?;
                if self.app.should_quit {
                    self.closed = true;
                    return Ok(DriverResponse::Closed);
                }
                if self
                    .app
                    .last_prompt_ack
                    .as_ref()
                    .is_some_and(|ack| ack.accepted)
                {
                    self.controller.replay_from_cursor(&self.app).await?;
                }
                self.sync().await?;
                if let Some(ack) = self.app.take_last_control_ack() {
                    ensure_command_accepted(&ack, "runtime rejected key command")?;
                }
                if let Some(ack) = self.app.take_last_prompt_ack() {
                    ensure_command_accepted(&ack, "runtime rejected the prompt")?;
                    self.record_submission(ack, after_sequence_no);
                }
                self.refresh_active_layout()?;
                Ok(DriverResponse::Accepted {
                    message: "key handled".to_owned(),
                })
            }
            DriverRequest::InputPaste { text } => {
                validate_input(&text)?;
                if self.app.task_id.is_some()
                    && self.app.overlay_surface() == Some(OverlaySurface::Auth)
                {
                    ensure_task_binding_accepts_no_control(
                        self.app.task_id,
                        "provider authentication input",
                    )?;
                }
                ensure_driver_input_capacity(&self.app, text.len())?;
                handle_paste(&text, &mut self.app);
                self.refresh_active_layout()?;
                Ok(DriverResponse::Accepted {
                    message: "paste handled".to_owned(),
                })
            }
            DriverRequest::InputMouse { event } => {
                if event.column >= self.width || event.row >= self.height {
                    return Ok(DriverResponse::Error {
                        code: "invalid_mouse_position".to_owned(),
                        message: format!(
                            "mouse position ({}, {}) is outside {}x{} viewport",
                            event.column, event.row, self.width, self.height
                        ),
                    });
                }
                let mut mouse = driver_mouse_event(event);
                let is_click = matches!(mouse.kind, crossterm::event::MouseEventKind::Down(_));
                ensure_driver_binding_allows_mouse_event(self.app.task_id, &self.app, &mouse)?;
                let mut activation = handle_mouse(mouse, &mut self.app);
                if is_click {
                    mouse.kind =
                        crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left);
                    activation = handle_mouse(mouse, &mut self.app).or(activation);
                }
                if let Some(activation) = activation {
                    ensure_driver_binding_allows_mouse(self.app.task_id, activation)?;
                    execute_mouse_activation(
                        activation,
                        &mut self.app,
                        self.controller.transport(),
                    )
                    .await?;
                    if let Some(ack) = self.app.take_last_control_ack() {
                        ensure_command_accepted(&ack, "runtime rejected mouse command")?;
                    }
                }
                self.refresh_active_layout()?;
                Ok(DriverResponse::Accepted {
                    message: "mouse event handled".to_owned(),
                })
            }
            DriverRequest::Resize { width, height } => {
                validate_dimensions(width, height)?;
                self.width = width;
                self.height = height;
                self.frame_cache.clear();
                self.refresh_active_layout()?;
                Ok(DriverResponse::Accepted {
                    message: "viewport resized".to_owned(),
                })
            }
            DriverRequest::Wait { until, timeout_ms } => self.wait_for(until, timeout_ms).await,
            DriverRequest::Snapshot { request } => {
                let frame = self.snapshot(request).await?;
                Ok(DriverResponse::Snapshot { frame })
            }
            DriverRequest::Takeover => {
                ensure_task_binding_accepts_no_control(self.app.task_id, "takeover")?;
                let ack = self
                    .app
                    .send_control_command(self.controller.transport(), SessionCommandKind::Takeover)
                    .await?;
                ensure_command_accepted(&ack, "runtime rejected controller takeover")?;
                self.last_controller_mode = DriverControllerMode::Controller;
                self.sync().await?;
                Ok(DriverResponse::Accepted {
                    message: "controller takeover requested".to_owned(),
                })
            }
            DriverRequest::Abort => {
                ensure_task_binding_accepts_no_control(self.app.task_id, "abort")?;
                let ack = self.app.abort(self.controller.transport()).await?;
                ensure_command_accepted(&ack, "runtime rejected abort")?;
                self.sync().await?;
                Ok(DriverResponse::Accepted {
                    message: "abort requested".to_owned(),
                })
            }
            DriverRequest::Close { abort_active_task } => {
                if abort_active_task {
                    ensure_task_binding_accepts_no_control(self.app.task_id, "abort before close")?;
                }
                if abort_active_task && has_active_task(&self.app) {
                    let ack = self.app.abort(self.controller.transport()).await?;
                    ensure_command_accepted(&ack, "runtime rejected abort before close")?;
                }
                self.closed = true;
                Ok(DriverResponse::Closed)
            }
        }
    }

    async fn handle_key_input(&mut self, key: DriverKey) -> miette::Result<()> {
        match key {
            DriverKey::Char(text) => {
                validate_input(&text)?;
                for character in text.chars() {
                    handle_key(
                        KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
                        &mut self.app,
                        self.controller.transport(),
                    )
                    .await?;
                }
            }
            DriverKey::CtrlC => {
                handle_key(
                    KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                    &mut self.app,
                    self.controller.transport(),
                )
                .await?;
            }
            key => {
                handle_key(
                    KeyEvent::new(driver_key_code(key), KeyModifiers::NONE),
                    &mut self.app,
                    self.controller.transport(),
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn wait_for(
        &mut self,
        until: WaitCondition,
        timeout_ms: Option<u64>,
    ) -> miette::Result<DriverResponse> {
        let started_at = self.start_wait_metrics();
        let result = self.wait_for_inner(until, timeout_ms).await;
        match &result {
            Ok(DriverResponse::WaitResult { .. }) => {
                self.finish_wait_metrics(started_at, false);
            }
            Ok(DriverResponse::WaitTimeout { .. }) => {
                self.finish_wait_metrics(started_at, true);
            }
            _ => self.cancel_wait_metrics(started_at),
        }
        result
    }

    async fn wait_for_inner(
        &mut self,
        until: WaitCondition,
        timeout_ms: Option<u64>,
    ) -> miette::Result<DriverResponse> {
        let timeout_ms = timeout_ms
            .unwrap_or(DEFAULT_WAIT_MILLIS)
            .min(MAX_WAIT_MILLIS);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        let submission = self.resolved_submission();
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                let state = self.wait_state(&until, submission);
                return Ok(DriverResponse::WaitTimeout {
                    condition: until,
                    state,
                });
            }
            let sync_budget = deadline
                .saturating_duration_since(now)
                .min(Duration::from_secs(1));
            match tokio::time::timeout(sync_budget, self.sync()).await {
                Ok(result) => result?,
                Err(_) => continue,
            }
            if self.condition_met_for(&until, submission) {
                let state = self.wait_state(&until, submission);
                return Ok(DriverResponse::WaitResult {
                    condition: until,
                    state,
                });
            }
            if tokio::time::Instant::now() >= deadline {
                let state = self.wait_state(&until, submission);
                return Ok(DriverResponse::WaitTimeout {
                    condition: until,
                    state,
                });
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    fn condition_met_for(
        &mut self,
        condition: &WaitCondition,
        submission: Option<SubmissionAnchor>,
    ) -> bool {
        self.wait_facts().condition_met(condition, submission)
    }

    fn wait_facts(&mut self) -> Arc<WaitFacts> {
        let key = WaitFactsCacheKey {
            event_count: self.app.events.len(),
            cursor: self.app.cursor,
            status: self
                .app
                .projection
                .as_ref()
                .map(|projection| projection.status),
            task_id: self.app.task_id.or_else(|| {
                self.app
                    .projection
                    .as_ref()
                    .and_then(|projection| projection.task_id)
            }),
            projection_ready: self.app.projection.is_some(),
        };
        if let Some(cached) = &self.wait_facts_cache
            && cached.key == key
        {
            return Arc::clone(&cached.facts);
        }
        let facts = Arc::new(WaitFacts::from_app(&self.app));
        self.wait_facts_cache = Some(CachedWaitFacts {
            key,
            facts: Arc::clone(&facts),
        });
        facts
    }

    fn condition_met_with_facts(
        &self,
        facts: &WaitFacts,
        condition: &WaitCondition,
        submission: Option<SubmissionAnchor>,
    ) -> bool {
        facts.condition_met(condition, submission)
    }

    async fn snapshot(&mut self, request: SnapshotRequest) -> miette::Result<TuiFrame> {
        let started_at = self.metrics.record_snapshot_request();
        let result = self.snapshot_inner(request).await;
        self.metrics.finish_snapshot(started_at);
        result
    }

    async fn snapshot_inner(&mut self, request: SnapshotRequest) -> miette::Result<TuiFrame> {
        request
            .validate()
            .map_err(|error| miette::miette!("invalid_snapshot: {error}"))?;
        if request.width != self.width || request.height != self.height {
            return Err(miette::miette!(
                "viewport_mismatch: snapshot dimensions must match the active {}x{} viewport; send resize first",
                self.width,
                self.height
            ));
        }
        self.prune_frames();
        if let Some(frame_id) = request.frame_id.as_deref() {
            let result = self.frozen_frame(frame_id, &request);
            self.metrics.record_frozen_frame_lookup(result.is_ok());
            return result;
        }
        self.sync().await?;
        let saved_developer_projection = self.app.developer_projection.clone();
        let saved_developer_error = self.app.developer_error.clone();
        if panes_include_developer(request.panes, self.app.debug_mode) {
            let projection_task = if matches!(
                request.scope,
                SnapshotScope::CurrentTurn | SnapshotScope::Task
            ) {
                current_task_and_turn(&self.app).0
            } else {
                self.app.task_id
            };
            let mut projection = load_debug_projection(
                self.controller.transport(),
                self.app.session_id,
                projection_task,
            )
            .await
            .map_err(|error| miette::miette!("debug_projection: {error}"))?;
            replace_debug_event_history(&mut projection, self.app.events.clone());
            self.app.developer_projection = Some(projection);
            self.app.developer_error = None;
        }
        let rendered = self.render_frame(&request);
        self.app.developer_projection = saved_developer_projection;
        self.app.developer_error = saved_developer_error;
        let active_layout = self.refresh_active_layout();
        let full_frame = rendered?;
        active_layout?;
        self.metrics.record_snapshot_render();
        cache_frame(
            &mut self.frame_cache,
            full_frame.clone(),
            tokio::time::Instant::now(),
        );
        slice_frame(full_frame, request.rows)
    }

    fn frozen_frame(&self, frame_id: &str, request: &SnapshotRequest) -> miette::Result<TuiFrame> {
        let frame = self
            .frame_cache
            .iter()
            .find(|cached| cached.frame.frame_id == frame_id)
            .map(|cached| cached.frame.clone())
            .ok_or_else(|| miette::miette!("frame_expired: frozen frame is unavailable"))?;
        if frame.width != request.width || frame.height != request.height {
            return Err(miette::miette!(
                "frame_mismatch: dimensions differ from the frozen frame"
            ));
        }
        if frame.scope != request.scope
            || frame.panes != request.panes
            || frame.cells.is_some() != matches!(request.detail, SnapshotDetail::Cells)
        {
            return Err(miette::miette!(
                "frame_mismatch: scope, panes, or detail differ from the frozen frame"
            ));
        }
        slice_frame(frame, request.rows)
    }

    fn prune_frames(&mut self) {
        prune_expired_frames(&mut self.frame_cache, tokio::time::Instant::now());
    }

    fn render_frame(&mut self, request: &SnapshotRequest) -> miette::Result<TuiFrame> {
        let saved_events = self.app.events.clone();
        let saved_ui = SnapshotUiState::capture(&self.app);
        let saved_developer = self.app.developer_projection.clone();
        let saved_commands = saved_ui.command_messages.clone();
        let saved_debug = self.app.debug_mode;
        let saved_body_view_mode = self.app.body_view_mode;
        let saved_transcript_scroll = self.app.transcript.scroll;
        let saved_transcript_top_row_override = self.app.transcript.top_row_override;
        let saved_transcript_revision = self.app.transcript.revision;
        let saved_transcript_layout_cache = self.app.transcript.layout_cache.clone();
        let saved_layout = self.app.layout;
        let saved_activity_projection = self.app.activity_projection.clone();
        let saved_change_projection = self.app.change_projection.clone();

        let scoped_events = scoped_runtime_events(&saved_events, request.scope);
        let scoped_developer = saved_developer
            .as_ref()
            .cloned()
            .map(|projection| scoped_debug_projection(projection, request.scope))
            .transpose()?;
        self.app.activity_projection.rebuild(&scoped_events);
        self.app.change_projection.rebuild(&scoped_events);
        self.app.events = scoped_events;
        self.app.developer_projection = scoped_developer;
        if matches!(
            request.scope,
            SnapshotScope::CurrentTurn | SnapshotScope::Task
        ) {
            self.app.command_messages.clear();
        } else {
            self.app.command_messages = saved_commands.clone();
        }
        self.app.debug_mode = match request.panes {
            SnapshotPanes::Transcript => false,
            SnapshotPanes::Developer | SnapshotPanes::ResponseAndDeveloper => true,
            SnapshotPanes::FullScreen => saved_debug,
        };
        self.app.body_view_mode = match request.panes {
            SnapshotPanes::Transcript => BodyViewMode::Transcript,
            SnapshotPanes::Developer => BodyViewMode::Developer,
            SnapshotPanes::ResponseAndDeveloper => BodyViewMode::Split,
            SnapshotPanes::FullScreen => saved_body_view_mode,
        };
        redact_snapshot_ui_state(&mut self.app);
        self.app.invalidate_transcript_layout();
        if !matches!(request.scope, SnapshotScope::Screen) {
            self.app.transcript.scroll.reset(0);
        }

        let rendered = (|| -> miette::Result<_> {
            let mut terminal = Terminal::new(TestBackend::new(request.width, request.height))
                .map_err(|error| miette::miette!("{error}"))?;
            terminal
                .draw(|frame| draw_ui(frame, &mut self.app))
                .map_err(|error| miette::miette!("{error}"))?;
            let layout = self.app.layout;
            let area = snapshot_area(request.panes, layout, request.width, request.height)?;
            let buffer = terminal.backend().buffer();
            let lines = frame_lines(buffer, area, request.panes);
            let cells = matches!(request.detail, SnapshotDetail::Cells)
                .then(|| frame_cells(buffer, area, request.panes));
            let hit_regions = frame_hit_regions(layout, area, &self.app);
            let scope_ids = current_task_and_turn(&self.app);
            let completeness = snapshot_completeness(&self.app, request.scope, request.panes);
            Ok((lines, cells, hit_regions, scope_ids, completeness))
        })();

        self.app.events = saved_events;
        self.app.developer_projection = saved_developer;
        saved_ui.restore(&mut self.app);
        self.app.transcript.scroll = saved_transcript_scroll;
        self.app.transcript.top_row_override = saved_transcript_top_row_override;
        self.app.transcript.revision = saved_transcript_revision;
        self.app.transcript.layout_cache = saved_transcript_layout_cache;
        self.app.debug_mode = saved_debug;
        self.app.body_view_mode = saved_body_view_mode;
        self.app.layout = saved_layout;
        self.app.activity_projection = saved_activity_projection;
        self.app.change_projection = saved_change_projection;

        let (lines, cells, hit_regions, (task_id, turn_id), (complete, mut missing_sections)) =
            rendered?;
        if matches!(request.scope, SnapshotScope::CurrentTurn) && turn_id.is_none() {
            missing_sections.push("current_turn".to_owned());
        }
        missing_sections.sort();
        missing_sections.dedup();
        let total_rows = lines.len().min(u32::MAX as usize) as u32;
        let mut frame = TuiFrame {
            frame_id: String::new(),
            instance_id: self.instance_id.clone(),
            workspace_id: self.controller.transport().workspace_id().to_string(),
            session_id: self.app.session_id.to_string(),
            task_id: task_id.map(|id| id.to_string()),
            turn_id: turn_id.map(|id| id.to_string()),
            event_high_watermark: self.app.cursor,
            width: request.width,
            height: request.height,
            scope: request.scope,
            panes: request.panes,
            total_rows,
            returned_range: RowRange {
                start: u32::from(total_rows > 0),
                end: total_rows,
            },
            lines,
            complete: complete && missing_sections.is_empty(),
            missing_sections,
            redaction_status: RedactionStatus::Redacted,
            next_range: None,
            hit_regions,
            cells,
        };
        frame.frame_id = frame_digest(&frame)?;
        Ok(frame)
    }

    fn refresh_active_layout(&mut self) -> miette::Result<()> {
        let mut terminal = Terminal::new(TestBackend::new(self.width, self.height))
            .map_err(|error| miette::miette!("render_layout: {error}"))?;
        terminal
            .draw(|frame| draw_ui(frame, &mut self.app))
            .map_err(|error| miette::miette!("render_layout: {error}"))?;
        Ok(())
    }

    fn take_notification(&mut self) -> Option<DriverResponseEnvelope> {
        let status = self
            .app
            .projection
            .as_ref()
            .map(|projection| projection.status);
        if self.app.cursor != self.last_notified_cursor {
            self.last_notified_cursor = self.app.cursor;
            let status_changed = status != self.last_notified_status;
            self.last_notified_status = status;
            return Some(response(
                format!("event:{}", self.app.cursor.unwrap_or_default()),
                DriverResponse::Event {
                    event: DriverNotification {
                        kind: if status_changed && status.is_some_and(is_terminal_status) {
                            DriverNotificationKind::TaskTerminal
                        } else {
                            DriverNotificationKind::RuntimeEventAvailable
                        },
                        sequence_no: self.app.cursor,
                        status: status.map(Into::into),
                    },
                },
            ));
        }
        if status != self.last_notified_status {
            self.last_notified_status = status;
            return Some(response(
                format!("state:{}", self.app.cursor.unwrap_or_default()),
                DriverResponse::Event {
                    event: DriverNotification {
                        kind: if status.is_some_and(is_terminal_status) {
                            DriverNotificationKind::TaskTerminal
                        } else {
                            DriverNotificationKind::StateChanged
                        },
                        sequence_no: self.app.cursor,
                        status: status.map(Into::into),
                    },
                },
            ));
        }
        None
    }

    fn record_submission(&mut self, ack: CommandAck, after_sequence_no: Option<u64>) {
        self.submission = Some(SubmissionAnchor {
            command_id: ack.command_id,
            after_sequence_no,
            task_id: None,
            turn_id: None,
        });
        self.resolve_submission_anchor();
    }

    fn resolve_submission_anchor(&mut self) {
        let Some(anchor) = self.submission else {
            return;
        };
        self.submission = Some(WaitFacts::from_app(&self.app).resolve_anchor(anchor));
    }

    fn resolved_submission(&mut self) -> Option<SubmissionAnchor> {
        let facts = self.wait_facts();
        self.submission.map(|anchor| facts.resolve_anchor(anchor))
    }
}

#[cfg(test)]
fn is_quiescent_status(status: TaskStatus) -> bool {
    status == TaskStatus::Idle || is_terminal_status(status)
}

fn ensure_session_binding_is_immutable(text: &str) -> miette::Result<()> {
    if matches!(
        parse_slash_input(text),
        SlashInput::Command(
            SlashCommand::New | SlashCommand::Resume { .. } | SlashCommand::Fork { .. }
        )
    ) {
        return Err(miette::miette!(
            "session_binding_immutable: a TUI Driver cannot switch sessions; start another Driver bound to the target session"
        ));
    }
    Ok(())
}

fn pending_slash_completion(app: &TuiApp) -> Option<SlashCommandCandidate> {
    let candidates = app.slash_candidates();
    let candidate = candidates
        .get(app.slash_selected.min(candidates.len().saturating_sub(1)))
        .cloned()?;
    let already_has_arguments = app
        .input
        .text()
        .trim()
        .starts_with(&format!("{} ", candidate.command));
    (!already_has_arguments).then_some(candidate)
}

fn task_and_turn_for_scope(
    app: &TuiApp,
    scope: WaitResponseScope,
) -> (Option<TaskId>, Option<TurnId>) {
    match scope {
        WaitResponseScope::Current => current_task_and_turn(app),
        WaitResponseScope::Submission { anchor, .. } => (anchor.task_id, anchor.turn_id),
    }
}

fn ensure_slash_input_is_valid(text: &str) -> miette::Result<()> {
    if let SlashInput::Error(error) = parse_slash_input(text) {
        return Err(miette::miette!("invalid_slash_command: {error}"));
    }
    Ok(())
}

fn ensure_modal_allows_slash(app: &TuiApp, command: &SlashCommand) -> miette::Result<()> {
    let modal_active = app.auth_operation.is_some() || app.overlay_surface().is_some();
    let can_resolve_runtime_wait = matches!(
        command,
        SlashCommand::Takeover
            | SlashCommand::Abort
            | SlashCommand::Pause
            | SlashCommand::Continue
            | SlashCommand::Approve
            | SlashCommand::Deny
    );
    if modal_active && !can_resolve_runtime_wait && !matches!(command, SlashCommand::Quit) {
        return Err(miette::miette!(
            "ui_modal_active: close the active TUI flow before executing another slash command"
        ));
    }
    Ok(())
}

fn ensure_task_binding_accepts_no_prompt(task_id: Option<TaskId>) -> miette::Result<()> {
    if let Some(task_id) = task_id {
        return Err(miette::miette!(
            "task_binding_read_only: Driver is fixed to task {task_id}; start a session-bound Driver without --task-id to submit prompts"
        ));
    }
    Ok(())
}

fn ensure_task_binding_accepts_no_control(
    task_id: Option<TaskId>,
    action: &str,
) -> miette::Result<()> {
    if let Some(task_id) = task_id {
        return Err(miette::miette!(
            "task_binding_read_only: Driver is fixed to task {task_id}; {action} requires a session-bound Driver without --task-id"
        ));
    }
    Ok(())
}

fn ensure_task_binding_allows_slash(task_id: Option<TaskId>, text: &str) -> miette::Result<()> {
    let Some(task_id) = task_id else {
        return Ok(());
    };
    let SlashInput::Command(command) = parse_slash_input(text) else {
        return Ok(());
    };
    let read_only = matches!(
        command,
        SlashCommand::Help
            | SlashCommand::WhatsNew
            | SlashCommand::Export
            | SlashCommand::Threads { .. }
            | SlashCommand::Status
            | SlashCommand::Plan
            | SlashCommand::Tasks
            | SlashCommand::Usage
            | SlashCommand::Debug(_)
            | SlashCommand::Clear
            | SlashCommand::Quit
            | SlashCommand::Auth(SlashAuthCommand::Status | SlashAuthCommand::Protocols)
    );
    if read_only {
        return Ok(());
    }
    Err(miette::miette!(
        "task_binding_read_only: Driver is fixed to task {task_id}; slash controls require a session-bound Driver without --task-id"
    ))
}

fn ensure_driver_binding_allows_key(
    task_id: Option<TaskId>,
    app: &TuiApp,
    key: &DriverKey,
) -> miette::Result<()> {
    let surface = app.overlay_surface();
    if surface == Some(OverlaySurface::Resume) && matches!(key, DriverKey::Enter) {
        return Err(miette::miette!(
            "session_binding_immutable: a TUI Driver cannot switch sessions; start another Driver bound to the target session"
        ));
    }

    let Some(task_id) = task_id else {
        return Ok(());
    };
    if matches!(key, DriverKey::CtrlC) {
        return ensure_task_binding_accepts_no_control(Some(task_id), "key control");
    }

    let runtime_control = match surface {
        Some(OverlaySurface::Auth) => matches!(key, DriverKey::Enter | DriverKey::Char(_)),
        Some(OverlaySurface::Approval) => driver_key_submits_approval_dialog(key),
        Some(OverlaySurface::Question) => matches!(key, DriverKey::Enter),
        Some(OverlaySurface::Queue) => driver_key_cancels_queued_turn(key),
        Some(
            OverlaySurface::Help
            | OverlaySurface::Resume
            | OverlaySurface::Dashboard
            | OverlaySurface::Settings
            | OverlaySurface::Export,
        ) => false,
        None if app.history_search.is_some() || app.transcript.search.is_some() => false,
        None => {
            let approval_shortcut = app.input.is_empty()
                && app
                    .projection
                    .as_ref()
                    .and_then(|projection| projection.pending_approval.as_ref())
                    .is_some()
                && driver_key_starts_approval_shortcut(key);
            let escape_interrupt = matches!(key, DriverKey::Escape)
                && has_active_task(app)
                && app.editing_queued_turn.is_none()
                && app.composer_mode != ComposerMode::VimInsert;
            approval_shortcut || escape_interrupt
        }
    };
    if runtime_control {
        return ensure_task_binding_accepts_no_control(Some(task_id), "key control");
    }
    Ok(())
}

fn ensure_driver_binding_allows_mouse(
    task_id: Option<TaskId>,
    activation: UiMouseActivation,
) -> miette::Result<()> {
    if activation == UiMouseActivation::ResumeSession {
        return Err(miette::miette!(
            "session_binding_immutable: a TUI Driver cannot switch sessions; start another Driver bound to the target session"
        ));
    }
    if task_id.is_some()
        && matches!(
            activation,
            UiMouseActivation::AuthContinue
                | UiMouseActivation::Approval(_)
                | UiMouseActivation::QuestionSubmit
        )
    {
        return ensure_task_binding_accepts_no_control(task_id, "mouse control");
    }
    Ok(())
}

fn ensure_driver_binding_allows_mouse_event(
    task_id: Option<TaskId>,
    app: &TuiApp,
    mouse: &crossterm::event::MouseEvent,
) -> miette::Result<()> {
    if !matches!(
        mouse.kind,
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left)
    ) {
        return Ok(());
    }
    let activation = match mouse_press_at(app, mouse.column, mouse.row) {
        Some(UiMousePress::Auth(_)) => Some(UiMouseActivation::AuthContinue),
        Some(UiMousePress::Resume(_)) if app.resume_picker.is_some() => {
            Some(UiMouseActivation::ResumeSession)
        }
        Some(UiMousePress::Approval(choice)) => Some(UiMouseActivation::Approval(choice)),
        Some(UiMousePress::QuestionSubmit)
            if app
                .question_dialog
                .as_ref()
                .is_some_and(QuestionDialogState::all_answered) =>
        {
            Some(UiMouseActivation::QuestionSubmit)
        }
        _ => None,
    };
    activation.map_or(Ok(()), |activation| {
        ensure_driver_binding_allows_mouse(task_id, activation)
    })
}

fn driver_enter_reaches_composer(app: &TuiApp) -> bool {
    app.overlay_surface().is_none()
        && app.history_search.is_none()
        && app.transcript.search.is_none()
}

fn driver_key_submits_approval_dialog(key: &DriverKey) -> bool {
    matches!(key, DriverKey::Enter)
        || matches!(
            key,
            DriverKey::Char(value)
                if value.chars().any(|character| matches!(character, '1' | '2' | '3' | '4' | 'y' | 'p' | 'a' | 'n'))
        )
}

fn driver_key_cancels_queued_turn(key: &DriverKey) -> bool {
    matches!(key, DriverKey::Delete | DriverKey::Backspace)
        || matches!(key, DriverKey::Char(value) if value.contains('d'))
}

fn driver_key_starts_approval_shortcut(key: &DriverKey) -> bool {
    matches!(
        key,
        DriverKey::Char(value) if matches!(value.chars().next(), Some('y' | 'n'))
    )
}

fn ensure_driver_input_capacity(app: &TuiApp, additional_bytes: usize) -> miette::Result<()> {
    if driver_input_state_bytes(app).saturating_add(additional_bytes) > MAX_DRIVER_INPUT_BYTES {
        return Err(miette::miette!(
            "input_too_large: accumulated input exceeds {MAX_DRIVER_INPUT_BYTES} UTF-8 bytes"
        ));
    }
    Ok(())
}

fn driver_input_state_bytes(app: &TuiApp) -> usize {
    let mut bytes = app.input.text().len();
    if let Some(dialog) = &app.auth_dialog {
        for value in [
            &dialog.base_url,
            &dialog.model,
            &dialog.api_key,
            &dialog.api_key_env,
            &dialog.context_window_size,
            &dialog.max_tokens,
            &dialog.custom_headers,
        ] {
            bytes = bytes.saturating_add(value.len());
        }
    }
    if let Some(dialog) = &app.question_dialog {
        bytes = bytes.saturating_add(dialog.input_bytes());
    }
    if let Some(picker) = &app.resume_picker {
        bytes = bytes.saturating_add(picker.input_bytes());
    }
    if let Some(dialog) = &app.settings_dialog {
        bytes = bytes.saturating_add(dialog.model_input.text().len());
    }
    if let Some(flow) = &app.export_flow {
        bytes = bytes.saturating_add(flow.input_bytes());
    }
    if let Some(search) = &app.history_search {
        bytes = bytes.saturating_add(search.input.text().len());
    }
    if let Some(search) = &app.transcript.search {
        bytes = bytes.saturating_add(search.input.text().len());
    }
    if let Some(stash) = &app.prompt_stash {
        bytes = bytes.saturating_add(stash.len());
    }
    bytes
}

fn redact_snapshot_ui_state(app: &mut TuiApp) {
    app.input.set_text(redacted_ui_text(app.input.text()));
    if let Some(projection) = &mut app.projection {
        redact_user_projection(projection);
    }
    for item in &mut app.command_messages {
        item.title = redacted_ui_text(&item.title);
        for line in &mut item.body {
            *line = redacted_ui_text(line);
        }
    }
    app.status_message = redacted_ui_text(&app.status_message);
    app.provider_message = redacted_ui_text(&app.provider_message);
    app.developer_error = app.developer_error.as_deref().map(redacted_ui_text);

    if let Some(dialog) = &mut app.auth_dialog {
        dialog.base_url = redacted_ui_text(&dialog.base_url);
        dialog.model = redacted_ui_text(&dialog.model);
        if !dialog.api_key.is_empty() {
            dialog.api_key = "<redacted-secret>".to_owned();
        }
        dialog.api_key_env = redacted_ui_text(&dialog.api_key_env);
        dialog.context_window_size = redacted_ui_text(&dialog.context_window_size);
        dialog.max_tokens = redacted_ui_text(&dialog.max_tokens);
        if !dialog.custom_headers.is_empty() {
            dialog.custom_headers = "<redacted-provider-headers>".to_owned();
        }
        dialog.error = dialog.error.as_deref().map(redacted_ui_text);
        if let Some(review) = &mut dialog.review {
            review.profile = redacted_ui_text(&review.profile);
            review.protocol = redacted_ui_text(&review.protocol);
            review.base_url = redacted_ui_text(&review.base_url);
            review.model = redacted_ui_text(&review.model);
            review.credential = "<redacted-secret-reference>".to_owned();
            review.advanced = redacted_ui_text(&review.advanced);
            review.config_path = redacted_ui_text(&review.config_path.display().to_string()).into();
            review.preview_json = "<redacted-provider-config>".to_owned();
        }
    }

    if let Some(picker) = &mut app.resume_picker {
        redact_session_picker(picker);
    }
    if let Some(picker) = &mut app.queue_picker {
        for item in &mut picker.items {
            item.prompt = redacted_ui_text(&item.prompt);
        }
    }
    if let Some(dialog) = &mut app.approval_dialog {
        dialog.request.tool_name = redacted_ui_text(&dialog.request.tool_name);
        dialog.request.resource = redacted_ui_text(&dialog.request.resource);
        dialog.request.reason = redacted_ui_text(&dialog.request.reason);
        dialog.resource_prefix = redacted_ui_text(&dialog.resource_prefix);
    }
    if let Some(dialog) = &mut app.question_dialog {
        dialog.redact_text_with(redacted_ui_text);
    }
    if let Some(dialog) = &mut app.settings_dialog {
        dialog.redact_text_with(redacted_ui_text);
    }
    if let Some(flow) = &mut app.export_flow {
        redact_session_picker(&mut flow.picker);
        let range_input = redacted_ui_text(flow.range_input.text());
        let destination_input = redacted_ui_text(flow.destination_input.text());
        flow.range_input.set_text(range_input);
        flow.destination_input.set_text(destination_input);
        flow.error = flow.error.as_deref().map(redacted_ui_text);
        if let Some(receipt) = &mut flow.receipt {
            receipt.destination =
                redacted_ui_text(&receipt.destination.display().to_string()).into();
        }
    }
    if let Some(search) = &mut app.history_search {
        search.input.set_text(redacted_ui_text(search.input.text()));
    }
    if let Some(search) = &mut app.transcript.search {
        search.input.set_text(redacted_ui_text(search.input.text()));
    }
    for attachment in &mut app.attachments {
        attachment.display_path = redacted_ui_text(&attachment.display_path);
    }
    if let Some(completion) = &mut app.mention_completion {
        for candidate in &mut completion.candidates {
            candidate.label = redacted_ui_text(&candidate.label);
            candidate.insertion = redacted_ui_text(&candidate.insertion);
            candidate.detail = redacted_ui_text(&candidate.detail);
        }
    }
    app.runtime_controls.redact_text_with(redacted_ui_text);
}

fn redact_user_projection(projection: &mut UserProjection) {
    for step in &mut projection.visible_steps {
        step.label = redacted_ui_text(&step.label);
        step.status = redacted_ui_text(&step.status);
        step.summary = redacted_ui_text(&step.summary);
    }
    projection.pending_approval = projection.pending_approval.as_deref().map(redacted_ui_text);
    projection.final_message = projection.final_message.as_deref().map(redacted_ui_text);
    for risk in &mut projection.residual_risks {
        *risk = redacted_ui_text(risk);
    }
}

fn redact_session_picker(picker: &mut ResumePickerState) {
    picker.redact_text_with(redacted_ui_text);
}

fn ensure_command_accepted(ack: &CommandAck, fallback: &str) -> miette::Result<()> {
    if ack.accepted {
        return Ok(());
    }
    Err(miette::miette!(
        "command_rejected: {}",
        ack.reason.as_deref().unwrap_or(fallback)
    ))
}

fn cache_frame(
    cache: &mut VecDeque<CachedFrame>,
    frame: TuiFrame,
    created_at: tokio::time::Instant,
) {
    cache.push_back(CachedFrame { created_at, frame });
    while cache.len() > FRAME_CACHE_CAPACITY {
        cache.pop_front();
    }
}

fn prune_expired_frames(cache: &mut VecDeque<CachedFrame>, now: tokio::time::Instant) {
    cache.retain(|cached| now.duration_since(cached.created_at) <= FRAME_CACHE_TTL);
}

#[cfg(test)]
mod tests;
