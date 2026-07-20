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
        width: u16,
        height: u16,
    ) -> miette::Result<Self> {
        validate_dimensions(width, height)?;
        let (thread_id, session_id) = resolve_driver_session(session, &transport).await?;
        let task_id = parse_task_id(task_id)?;
        validate_task_id(task_id, session_id, &transport).await?;
        let provider_status = current_provider_ui_status();
        let runtime_cwd = transport
            .cwd()
            .map(Path::to_path_buf)
            .ok_or_else(|| miette::miette!("TUI driver transport has no workspace"))?;
        let auth_dialog = if task_id.is_some() {
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
        .with_footer_context(runtime_cwd, provider_status.model);
        let controller = TuiRuntimeController::attach(&mut app, transport).await?;
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
        driver.last_controller_mode = driver.controller_mode().await?;
        driver.refresh_active_layout()?;
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

    async fn state(&mut self) -> miette::Result<DriverState> {
        self.refresh_controller_mode().await;
        Ok(self.cached_state())
    }

    fn cached_state(&self) -> DriverState {
        let (task_id, turn_id) = current_task_and_turn(&self.app);
        DriverState {
            instance_id: self.instance_id.clone(),
            thread_id: self.app.thread_id.to_string(),
            session_id: self.app.session_id.to_string(),
            task_id: task_id.map(|id| id.to_string()),
            turn_id: turn_id.map(|id| id.to_string()),
            status: self
                .app
                .projection
                .as_ref()
                .map_or(DriverTaskStatus::Connecting, |projection| {
                    projection.status.into()
                }),
            width: self.width,
            height: self.height,
            facts_expanded: self.app.developer_facts_expanded,
            controller_mode: self.last_controller_mode,
            closed: self.closed,
        }
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
        Ok(projection
            .runtime_lane
            .map_or(DriverControllerMode::Controller, |lane| {
                if lane.active_controller.id == TUI_ACTOR_ID.as_str() {
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
        result
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
                ensure_task_binding_allows_key(self.app.task_id, &self.app, &key)?;
                if matches!(key, DriverKey::Enter)
                    && self.app.auth_dialog.is_none()
                    && self.app.resume_picker.is_none()
                    && self.app.export_flow.is_none()
                {
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
                if self.app.task_id.is_some() && self.app.auth_dialog.is_some() {
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
                handle_mouse(driver_mouse_event(event), &mut self.app);
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
                return Ok(DriverResponse::WaitTimeout {
                    condition: until,
                    state: self.cached_state(),
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
                return Ok(DriverResponse::WaitResult {
                    condition: until,
                    state: self.cached_state(),
                });
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(DriverResponse::WaitTimeout {
                    condition: until,
                    state: self.cached_state(),
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
        if panes_include_developer(request.panes, self.app.debug_mode) {
            let projection_task = if matches!(
                request.scope,
                SnapshotScope::CurrentTurn | SnapshotScope::Task
            ) {
                current_task_and_turn(&self.app).0
            } else {
                self.app.task_id
            };
            self.app.developer_projection = Some(
                load_debug_projection(
                    self.controller.transport(),
                    self.app.session_id,
                    projection_task,
                )
                .await
                .map_err(|error| miette::miette!("debug_projection: {error}"))?,
            );
            self.app.developer_error = None;
        }
        let full_frame = self.render_frame(&request)?;
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
        let saved_projection = self.app.projection.clone();
        let saved_developer = self.app.developer_projection.clone();
        let saved_commands = self.app.command_messages.clone();
        let saved_auth_dialog = self.app.auth_dialog.clone();
        let saved_resume_picker = self.app.resume_picker.clone();
        let saved_export_flow = self.app.export_flow.clone();
        let saved_status_message = self.app.status_message.clone();
        let saved_provider_message = self.app.provider_message.clone();
        let saved_developer_error = self.app.developer_error.clone();
        let saved_debug = self.app.debug_mode;
        let saved_transcript_scroll = self.app.transcript_scroll;
        let saved_developer_scroll = self.app.developer_scroll;
        let saved_layout = self.app.layout;
        let saved_input = self.app.input.clone();

        let scoped_events = scoped_event_values(&saved_events, request.scope);
        let scoped_developer = saved_developer
            .as_ref()
            .cloned()
            .map(|projection| scoped_debug_projection(projection, request.scope))
            .transpose()?;
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
        if matches!(request.panes, SnapshotPanes::FullScreen) && !saved_input.is_empty() {
            self.app
                .input
                .set_text(redacted_ui_text(saved_input.text()));
        }
        redact_snapshot_ui_state(&mut self.app);
        if !matches!(request.scope, SnapshotScope::Screen) {
            self.app.transcript_scroll.reset(0);
            self.app.developer_scroll.reset(0);
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
            let hit_regions = frame_hit_regions(layout, area);
            let scope_ids = current_task_and_turn(&self.app);
            let completeness = snapshot_completeness(&self.app, request.scope, request.panes);
            Ok((lines, cells, hit_regions, scope_ids, completeness, layout))
        })();

        self.app.events = saved_events;
        self.app.projection = saved_projection;
        self.app.developer_projection = saved_developer;
        self.app.command_messages = saved_commands;
        self.app.auth_dialog = saved_auth_dialog;
        self.app.resume_picker = saved_resume_picker;
        self.app.export_flow = saved_export_flow;
        self.app.status_message = saved_status_message;
        self.app.provider_message = saved_provider_message;
        self.app.developer_error = saved_developer_error;
        self.app.transcript_scroll = saved_transcript_scroll;
        self.app.developer_scroll = saved_developer_scroll;
        self.app.debug_mode = saved_debug;
        self.app.layout = saved_layout;
        self.app.input = saved_input;

        let (
            lines,
            cells,
            hit_regions,
            (task_id, turn_id),
            (complete, mut missing_sections),
            rendered_layout,
        ) = rendered?;
        self.app.layout = rendered_layout;
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

fn ensure_slash_input_is_valid(text: &str) -> miette::Result<()> {
    if let SlashInput::Error(error) = parse_slash_input(text) {
        return Err(miette::miette!("invalid_slash_command: {error}"));
    }
    Ok(())
}

fn ensure_modal_allows_slash(app: &TuiApp, command: &SlashCommand) -> miette::Result<()> {
    let modal_active = app.auth_operation.is_some()
        || app.auth_dialog.is_some()
        || app.resume_picker.is_some()
        || app.export_flow.is_some();
    if modal_active && !matches!(command, SlashCommand::Quit) {
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
            | SlashCommand::Export
            | SlashCommand::Threads { .. }
            | SlashCommand::Status
            | SlashCommand::Debug
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

fn ensure_task_binding_allows_key(
    task_id: Option<TaskId>,
    app: &TuiApp,
    key: &DriverKey,
) -> miette::Result<()> {
    if task_id.is_none() {
        return Ok(());
    }
    if app.auth_dialog.is_some() && matches!(key, DriverKey::Enter | DriverKey::Char(_)) {
        return ensure_task_binding_accepts_no_control(task_id, "provider authentication input");
    }
    let approval_shortcut = app.input.is_empty()
        && app
            .projection
            .as_ref()
            .and_then(|projection| projection.pending_approval.as_ref())
            .is_some()
        && driver_key_starts_approval_shortcut(key);
    if matches!(key, DriverKey::CtrlC) || approval_shortcut {
        return ensure_task_binding_accepts_no_control(task_id, "key control");
    }
    Ok(())
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
    if let Some(flow) = &app.export_flow {
        bytes = bytes
            .saturating_add(flow.range_input.len())
            .saturating_add(flow.destination_input.len());
    }
    bytes
}

fn redact_snapshot_ui_state(app: &mut TuiApp) {
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
        if !dialog.api_key.is_empty() {
            dialog.api_key = "<redacted-secret>".to_owned();
        }
        if !dialog.custom_headers.is_empty() {
            dialog.custom_headers = "<redacted-provider-headers>".to_owned();
        }
        dialog.error = dialog.error.as_deref().map(redacted_ui_text);
        if let Some(review) = &mut dialog.review {
            review.base_url = redacted_ui_text(&review.base_url);
            review.credential = "<redacted-secret-reference>".to_owned();
            review.advanced = redacted_ui_text(&review.advanced);
            review.preview_json = "<redacted-provider-config>".to_owned();
        }
    }

    if let Some(picker) = &mut app.resume_picker {
        redact_session_picker(picker);
    }
    if let Some(flow) = &mut app.export_flow {
        redact_session_picker(&mut flow.picker);
        flow.range_input = redacted_ui_text(&flow.range_input);
        flow.destination_input = redacted_ui_text(&flow.destination_input);
        flow.error = flow.error.as_deref().map(redacted_ui_text);
        if let Some(receipt) = &mut flow.receipt {
            receipt.destination =
                redacted_ui_text(&receipt.destination.display().to_string()).into();
        }
    }
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
    for item in &mut picker.items {
        item.title = redacted_ui_text(&item.title);
        item.preview = redacted_ui_text(&item.preview);
    }
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
