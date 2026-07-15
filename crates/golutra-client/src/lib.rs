use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use fs2::FileExt;
use golutra_context::ContextContributor;
use golutra_core::{
    Actor, ApprovalDecision, ApprovalId, ApprovalResolution, ArtifactId, ArtifactRecord,
    BusyPolicy, EventId, MemoryId, RedactionStatus, SessionId, TaskId, TaskStatus, ThreadId,
    TurnId, WorkspaceId,
};
use golutra_eval::{
    EvaluationError, EvaluationRunner, EvaluationStore, PromotionDecisionKind,
    TaskEvaluationBundle, TaskEvaluationInput,
};
use golutra_llm::ProviderRole;
use golutra_memory::{MemoryError, MemoryPromotionGate, MemoryStore, propose_project_memory};
use golutra_policy::WorkspacePolicy;
use golutra_protocol::{
    CommandAck, EventFilter, RuntimeEvent, RuntimeEventSource, RuntimeEventType, RuntimeQuery,
    RuntimeQueryKind, SessionCommand, SessionCommandKind,
};
use golutra_runtime::{
    AgentExecutionControl, AgentExecutionHandle, AgentLoop, AgentLoopError, AgentLoopTraceEvent,
    AgentTaskRequest, BeforeSideEffectRecorder, PendingAgentTurn, RuntimeLaneError,
    RuntimeLaneManager, WorkspaceCheckpointManager, agent_execution_channel, is_active_status,
};
use golutra_store::{RuntimeStore, StoreError, ThreadRecord};
use golutra_tools::{BasicToolExecutor, FileBeforeImage, ToolRequest};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    sync::{Mutex, broadcast, mpsc, oneshot, watch},
    task::AbortHandle,
};
use uuid::Uuid;

pub const APP_SERVER_ATTACHMENT_HEADER: &str = "x-golutra-attachment";
const PROVISIONAL_COMMAND_ACK_REASON: &str = "command accepted for processing";
const EVENT_REPLAY_PAGE_SIZE: u32 = 256;
const MAX_HISTORY_SOURCE_EVENTS: u32 = 512;
const MAX_HTTP_JSON_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_SSE_EVENT_BYTES: usize = 1024 * 1024;
const MAX_COMMAND_PAYLOAD_JSON_BYTES: usize = 256 * 1024;
const MAX_IDEMPOTENCY_KEY_CHARS: usize = 512;
const MAX_ACTOR_ID_CHARS: usize = 256;
const MAX_ROLLOUT_LINE_BYTES: usize = 20 * 1024 * 1024;
const ROLLOUT_FORMAT_VERSION: u32 = 1;

mod context;
mod event_codec;
mod paths;
mod provider_runtime;
mod rollout;
mod transport;

pub(crate) use context::{
    compact_event_summary, compact_history_text, compact_history_with_summary,
    conversation_history_line, environment_context_prompt, explicit_compaction_from_event,
    load_project_instructions, memory_context, preview_from_payload, prompt_from_payload,
    system_prompt, title_from_payload,
};
#[cfg(test)]
pub(crate) use event_codec::redact_provider_json;
pub(crate) use event_codec::{
    agent_event, agent_event_for_turn, candidate_id_from_payload, event_matches_filter, host_event,
    provider_raw_artifact, recovered_pending_turn_from_event, task_status_from_loop_action,
    thread_id_from_payload, thread_title_for_prompt, trace_event_payload, with_command_payload,
};
pub use event_codec::{event_sequence_no, projection_status};
pub use paths::{AppServerPaths, RuntimePaths};
pub(crate) use paths::{ensure_private_dir, set_owner_only_file, workspace_hash};
pub(crate) use provider_runtime::{MockProviderPlan, mock_provider_plan};
#[cfg(test)]
pub(crate) use provider_runtime::{
    MockWriteFileArgs, configured_provider_plan, mock_write_file_args,
};
pub use rollout::{RolloutEnvelope, RolloutExport, ThreadRebindResult};
pub(crate) use rollout::{
    append_rollout_line, normalize_rebind_source, rebuild_rollout_file, rollout_line,
    rollout_path_for_workspace,
};
#[cfg(test)]
pub(crate) use rollout::{redact_rollout_value, rollout_lock_path};
pub(crate) use transport::run_blocking;
pub use transport::{
    AppServerInfo, EmbeddedTransport, HttpSseTransport, RuntimeAttachment, RuntimeClient,
    RuntimeEventStream, RuntimeHostInfo, RuntimeTransport,
};
#[cfg(test)]
pub(crate) use transport::{
    ParsedSseEvent, parse_sse_frame, sse_frame_complete, validate_local_app_server_base_url,
};

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("runtime store failed")]
    Store(#[from] StoreError),
    #[error("runtime lane failed")]
    RuntimeLane(#[from] RuntimeLaneError),
    #[error("query result serialization failed")]
    Serialization(#[from] serde_json::Error),
    #[error("runtime workspace io failed: {0}")]
    Io(String),
    #[error("runtime session id is invalid: {0}")]
    InvalidSession(String),
    #[error("runtime task execution failed: {0}")]
    TaskExecution(String),
    #[error("runtime task was cancelled")]
    TaskCancelled,
    #[error("runtime HTTP transport failed: {0}")]
    Http(String),
    #[error("runtime daemon failed: {0}")]
    Daemon(String),
    #[error("runtime memory failed")]
    Memory(#[from] MemoryError),
    #[error("runtime evaluation failed")]
    Evaluation(#[from] EvaluationError),
}

#[derive(Debug)]
pub struct RuntimeHost {
    store: RuntimeStore,
    memory_store: MemoryStore,
    evaluation_store: EvaluationStore,
    lane_manager: Mutex<RuntimeLaneManager>,
    event_bus: broadcast::Sender<RuntimeEvent>,
    next_sequence_no: AtomicU64,
    event_writer: Mutex<()>,
    workspace_id: WorkspaceId,
    workspace_root: Option<PathBuf>,
    runtime_paths: Option<RuntimePaths>,
    default_session_id: SessionId,
    default_thread_id: ThreadId,
    instance_id: String,
    started_at: chrono::DateTime<chrono::Utc>,
    command_mutex: Mutex<()>,
    task_controls: Mutex<HashMap<SessionId, HostedTaskControl>>,
}

#[derive(Debug, Clone)]
pub(crate) struct HostedAgentTask {
    pub(crate) session_id: SessionId,
    pub(crate) task_id: TaskId,
    pub(crate) turn_id: TurnId,
    pub(crate) payload: Value,
}

#[derive(Debug, Clone)]
pub(crate) struct RecoveredPendingTurn {
    pub(crate) sequence_no: u64,
    pub(crate) actor: Actor,
    pub(crate) payload: Value,
    pub(crate) pending: PendingAgentTurn,
}

struct HostedTaskEvaluation<'a> {
    objective: &'a str,
    task_status: TaskStatus,
    verification: Option<golutra_core::VerificationRecord>,
    tool_reports: &'a [golutra_tools::ToolExecutionReport],
    failure_summary: Option<String>,
    latency: Duration,
}

#[derive(Debug, Clone)]
struct HostedTaskControl {
    task_id: TaskId,
    execution: AgentExecutionHandle,
    abort_handle: AbortHandle,
    completion: watch::Receiver<bool>,
    _session_lease: Option<Arc<File>>,
}

enum SessionLeaseAttempt {
    Acquired(Option<Arc<File>>),
    Busy,
}

enum HostedTraceCommand {
    Event(Box<AgentLoopTraceEvent>),
    Flush(oneshot::Sender<Result<(), ClientError>>),
}

#[derive(Debug, Clone)]
struct HostedCheckpointRecorder {
    host: Arc<RuntimeHost>,
    task: HostedAgentTask,
    trace_sender: mpsc::UnboundedSender<HostedTraceCommand>,
}

#[async_trait]
impl BeforeSideEffectRecorder for HostedCheckpointRecorder {
    async fn persist_before_side_effect(
        &self,
        request: &ToolRequest,
        before_images: &[FileBeforeImage],
    ) -> Result<(), AgentLoopError> {
        let (flush_sender, flush_receiver) = oneshot::channel();
        self.trace_sender
            .send(HostedTraceCommand::Flush(flush_sender))
            .map_err(|_| AgentLoopError::Checkpoint("trace recorder is unavailable".to_owned()))?;
        flush_receiver
            .await
            .map_err(|_| AgentLoopError::Checkpoint("trace recorder stopped".to_owned()))?
            .map_err(|error| AgentLoopError::Checkpoint(error.to_string()))?;
        self.host
            .persist_checkpoint_before_side_effect(&self.task, request, before_images)
            .await
            .map_err(|error| AgentLoopError::Checkpoint(error.to_string()))
    }
}

impl RuntimeHost {
    pub async fn in_memory() -> Result<Arc<Self>, ClientError> {
        let store = RuntimeStore::in_memory().await?;
        let default_session_id = SessionId::new();
        let default_thread_id = ThreadId::new();
        Self::from_store(
            store,
            None,
            None,
            WorkspaceId::new(),
            default_session_id,
            default_thread_id,
        )
        .await
    }

    pub async fn for_cwd(cwd: impl AsRef<Path>) -> Result<Arc<Self>, ClientError> {
        let paths = RuntimePaths::for_cwd(cwd)?;
        Self::from_runtime_paths(paths).await
    }

    pub async fn from_home_and_cwd(
        home: impl AsRef<Path>,
        cwd: impl AsRef<Path>,
    ) -> Result<Arc<Self>, ClientError> {
        let paths = RuntimePaths::from_home_and_cwd(home, cwd)?;
        Self::from_runtime_paths(paths).await
    }

    async fn from_runtime_paths(paths: RuntimePaths) -> Result<Arc<Self>, ClientError> {
        let store = RuntimeStore::connect_with_artifact_root(
            &paths.sqlite_url(),
            paths.artifacts_dir.clone(),
        )
        .await?;
        set_owner_only_file(&paths.runtime_db)?;
        let cwd = paths.cwd.to_string_lossy().to_string();
        let latest_thread = store.list_threads(Some(&cwd), 1).await?.into_iter().next();
        let (default_session_id, default_thread_id) = latest_thread.map_or_else(
            || (SessionId::new(), ThreadId::new()),
            |thread| (thread.session_id, thread.thread_id),
        );
        let host = Self::from_store(
            store,
            Some(paths.cwd.clone()),
            Some(paths.clone()),
            paths.workspace_id(),
            default_session_id,
            default_thread_id,
        )
        .await?;
        host.synchronize_workspace_rollouts().await?;
        host.recover_orphaned_tasks().await?;
        Ok(host)
    }

    async fn from_store(
        store: RuntimeStore,
        workspace_root: Option<PathBuf>,
        runtime_paths: Option<RuntimePaths>,
        workspace_id: WorkspaceId,
        default_session_id: SessionId,
        default_thread_id: ThreadId,
    ) -> Result<Arc<Self>, ClientError> {
        let (event_bus, _) = broadcast::channel(512);
        let max_sequence_no = store.max_sequence_no().await?;
        let next_sequence_no = max_sequence_no.saturating_add(1);
        let memory_store = runtime_paths
            .as_ref()
            .map_or_else(MemoryStore::in_memory, |paths| {
                MemoryStore::new(paths.memory_file.clone())
            });
        let evaluation_store = runtime_paths
            .as_ref()
            .map_or_else(EvaluationStore::in_memory, |paths| {
                EvaluationStore::new(paths.evaluation_file.clone())
            });
        Ok(Arc::new(Self {
            store,
            memory_store,
            evaluation_store,
            lane_manager: Mutex::new(RuntimeLaneManager::new()),
            event_bus,
            next_sequence_no: AtomicU64::new(next_sequence_no),
            event_writer: Mutex::new(()),
            workspace_id,
            workspace_root,
            runtime_paths,
            default_session_id,
            default_thread_id,
            instance_id: Uuid::now_v7().to_string(),
            started_at: chrono::Utc::now(),
            command_mutex: Mutex::new(()),
            task_controls: Mutex::new(HashMap::new()),
        }))
    }

    #[must_use]
    pub fn default_session_id(&self) -> SessionId {
        self.default_session_id
    }

    #[must_use]
    pub fn default_thread_id(&self) -> ThreadId {
        self.default_thread_id
    }

    #[must_use]
    pub fn workspace_root(&self) -> Option<&Path> {
        self.workspace_root.as_deref()
    }

    pub async fn runtime_info(
        &self,
        base_url: impl Into<String>,
    ) -> Result<RuntimeHostInfo, ClientError> {
        let workspace_root = self.workspace_root_string();
        let latest_thread = self
            .store
            .list_threads(workspace_root.as_deref(), 1)
            .await?
            .into_iter()
            .next();
        let (default_session_id, default_thread_id) = latest_thread.map_or_else(
            || (self.default_session_id, self.default_thread_id),
            |thread| (thread.session_id, thread.thread_id),
        );
        Ok(RuntimeHostInfo {
            instance_id: self.instance_id.clone(),
            pid: std::process::id(),
            base_url: base_url.into(),
            cwd: self
                .workspace_root
                .as_ref()
                .map_or_else(String::new, |path| path.display().to_string()),
            workspace_id: self.workspace_id,
            default_session_id,
            default_thread_id,
            started_at: self.started_at,
        })
    }

    #[must_use]
    pub fn subscribe_live(&self, _filter: EventFilter) -> broadcast::Receiver<RuntimeEvent> {
        self.event_bus.subscribe()
    }

    async fn event_stream(
        self: Arc<Self>,
        filter: EventFilter,
    ) -> Result<RuntimeEventStream, ClientError> {
        self.ensure_session_in_workspace(filter.session_id).await?;
        let mut live = self.event_bus.subscribe();
        let (sender, receiver) = mpsc::channel(256);
        tokio::spawn(async move {
            let mut cursor = filter.after_sequence_no;
            match self.send_replay_pages(&filter, &mut cursor, &sender).await {
                Ok(true) => {}
                Ok(false) => return,
                Err(error) => {
                    let _ = sender.send(Err(error)).await;
                    return;
                }
            }
            loop {
                match live.recv().await {
                    Ok(event) if event_matches_filter(&event, &filter, cursor) => {
                        cursor = Some(event.sequence_no);
                        if sender.send(Ok(event)).await.is_err() {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        match self.send_replay_pages(&filter, &mut cursor, &sender).await {
                            Ok(true) => {}
                            Ok(false) => return,
                            Err(error) => {
                                let _ = sender.send(Err(error)).await;
                                return;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        Ok(RuntimeEventStream::new(receiver))
    }

    async fn send_replay_pages(
        &self,
        filter: &EventFilter,
        cursor: &mut Option<u64>,
        sender: &mpsc::Sender<Result<RuntimeEvent, ClientError>>,
    ) -> Result<bool, ClientError> {
        loop {
            let events = self
                .store
                .load_events_page(
                    filter.session_id,
                    filter.task_id,
                    *cursor,
                    EVENT_REPLAY_PAGE_SIZE,
                )
                .await?;
            let page_is_full = events.len() == EVENT_REPLAY_PAGE_SIZE as usize;
            for event in events {
                *cursor = Some(event.sequence_no);
                if sender.send(Ok(event)).await.is_err() {
                    return Ok(false);
                }
            }
            if !page_is_full {
                return Ok(true);
            }
        }
    }

    pub async fn recover_orphaned_tasks(self: &Arc<Self>) -> Result<usize, ClientError> {
        let Some(workspace_root) = self.workspace_root_string() else {
            return Ok(0);
        };
        let threads = self
            .store
            .list_threads(Some(&workspace_root), u32::MAX)
            .await?;
        let mut recovered = 0;
        for thread in threads {
            let state = self.store.query_state(thread.session_id, None).await?;
            let orphan_is_active = matches!(
                state.task_status,
                TaskStatus::Running
                    | TaskStatus::WaitingApproval
                    | TaskStatus::Pausing
                    | TaskStatus::Paused
                    | TaskStatus::Aborting
            );
            let may_have_pending_turns = state
                .runtime_lane
                .as_ref()
                .is_some_and(|lane| !lane.pending_turns.is_empty());
            if !orphan_is_active && !may_have_pending_turns {
                continue;
            }
            let SessionLeaseAttempt::Acquired(lease) =
                self.try_acquire_session_lease(state.session_id)?
            else {
                continue;
            };
            let pending_turns = self
                .recoverable_pending_turns(state.session_id, state.active_task_id)
                .await?;
            if orphan_is_active {
                self.record_orphaned_task_cancelled(
                    state.session_id,
                    state.active_task_id,
                    "runtime_process_restart",
                    "orphaned task cancelled during runtime host recovery",
                )
                .await?;
            }
            if !pending_turns.is_empty() {
                self.clone()
                    .restart_pending_turns(state.session_id, pending_turns, lease)
                    .await?;
            }
            recovered += 1;
        }
        Ok(recovered)
    }

    async fn recoverable_pending_turns(
        &self,
        session_id: SessionId,
        task_id: Option<TaskId>,
    ) -> Result<Vec<RecoveredPendingTurn>, ClientError> {
        let Some(task_id) = task_id else {
            return Ok(Vec::new());
        };
        let events = self
            .store
            .load_events(session_id, Some(task_id), None)
            .await?;
        let mut pending = HashMap::<TurnId, RecoveredPendingTurn>::new();
        for event in events {
            match event.event_type {
                RuntimeEventType::TurnQueued => {
                    let referenced_sequences = event
                        .payload
                        .get("recovered_pending_sequence_nos")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_u64)
                        .collect::<Vec<_>>();
                    for sequence_no in referenced_sequences {
                        if let Some(referenced) = self
                            .store
                            .load_event_by_sequence(session_id, sequence_no)
                            .await?
                            .and_then(|event| recovered_pending_turn_from_event(&event))
                        {
                            pending.insert(referenced.pending.turn_id, referenced);
                        }
                    }
                    if let Some(recovered) = recovered_pending_turn_from_event(&event) {
                        pending.insert(recovered.pending.turn_id, recovered);
                    }
                }
                RuntimeEventType::TurnStarted => {
                    if let Some(turn_id) = event.turn_id {
                        pending.remove(&turn_id);
                    }
                }
                _ => {}
            }
        }
        let mut pending = pending.into_values().collect::<Vec<_>>();
        pending.sort_by_key(|turn| turn.sequence_no);
        Ok(pending)
    }

    async fn restart_pending_turns(
        self: Arc<Self>,
        session_id: SessionId,
        mut pending_turns: Vec<RecoveredPendingTurn>,
        session_lease: Option<Arc<File>>,
    ) -> Result<(), ClientError> {
        if pending_turns.is_empty() {
            return Ok(());
        }
        let first = pending_turns.remove(0);
        let task_id = TaskId::new();
        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            Some(task_id),
            RuntimeEventType::TurnQueued,
            RuntimeEventSource::Runtime,
            json!({
                "summary": "durable pending turns transferred to a recovery task",
                "recovery": "durable_pending_turn_batch",
                "recovered_pending_sequence_nos": std::iter::once(&first)
                    .chain(pending_turns.iter())
                    .map(|turn| turn.sequence_no)
                    .collect::<Vec<_>>(),
            }),
        ))
        .await?;
        let mut lane_manager = self.lane_manager.lock().await;
        let mut transition = lane_manager.start_task(
            self.workspace_id,
            session_id,
            task_id,
            first.pending.turn_id,
            first.actor,
            self.next_sequence_no(),
        )?;
        for pending in &pending_turns {
            lane_manager.queue_turn(session_id, pending.pending.turn_id, 0)?;
        }
        if let Some(lane) = lane_manager.lane(session_id) {
            transition.event.payload["runtime_lane"] = json!(lane);
        }
        drop(lane_manager);
        transition.event.event_type = RuntimeEventType::TurnStarted;
        transition.event.payload["summary"] =
            json!("durable pending turn restarted after runtime recovery");
        transition.event.payload["recovery"] = json!("durable_pending_turn");
        transition.event.payload["command_id"] = json!(first.pending.command_id);
        self.record_event(transition.event).await?;
        self.spawn_agent_task(
            HostedAgentTask {
                session_id,
                task_id,
                turn_id: first.pending.turn_id,
                payload: first.payload,
            },
            session_lease,
            pending_turns.into_iter().map(|turn| turn.pending).collect(),
        )
        .await
    }

    async fn record_orphaned_task_cancelled(
        &self,
        session_id: SessionId,
        task_id: Option<TaskId>,
        recovery: &str,
        summary: &str,
    ) -> Result<(), ClientError> {
        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            task_id,
            RuntimeEventType::TaskAborted,
            RuntimeEventSource::Runtime,
            json!({
                "summary": summary,
                "status": TaskStatus::Cancelled,
                "recovery": recovery,
            }),
        ))
        .await
    }

    pub async fn handle_command(
        self: Arc<Self>,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let idempotency_key = command.idempotency_key.trim().to_owned();
        if idempotency_key.is_empty() {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some("idempotency_key is required".to_owned()),
            });
        }
        if idempotency_key.chars().count() > MAX_IDEMPOTENCY_KEY_CHARS {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some(format!(
                    "idempotency_key exceeds {MAX_IDEMPOTENCY_KEY_CHARS} characters"
                )),
            });
        }
        if command.actor.id.trim().is_empty()
            || command.actor.id.chars().count() > MAX_ACTOR_ID_CHARS
        {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some(format!(
                    "actor id must contain 1..={MAX_ACTOR_ID_CHARS} characters"
                )),
            });
        }
        let payload_size = serde_json::to_vec(&command.payload)?.len();
        if payload_size > MAX_COMMAND_PAYLOAD_JSON_BYTES {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some(format!(
                    "command payload exceeds {MAX_COMMAND_PAYLOAD_JSON_BYTES} serialized bytes"
                )),
            });
        }
        let scoped_idempotency_key = self.scoped_idempotency_key(&idempotency_key);
        let session_id = command.session_id.unwrap_or(self.default_session_id);
        self.ensure_session_in_workspace(session_id).await?;
        let _command_guard = self.command_mutex.lock().await;
        let _command_lease = self.acquire_command_lease(&scoped_idempotency_key).await?;
        let command_id = command.command_id;
        if let Some(existing_ack) = self.store.command_ack(&scoped_idempotency_key).await? {
            if existing_ack.command_id != command_id {
                return Ok(CommandAck {
                    command_id,
                    accepted: false,
                    reason: Some(format!(
                        "idempotency key is already assigned to command {}",
                        existing_ack.command_id
                    )),
                });
            }
            if existing_ack.reason.as_deref() != Some(PROVISIONAL_COMMAND_ACK_REASON) {
                return Ok(existing_ack);
            }
        }
        let provisional_ack = CommandAck {
            command_id,
            accepted: true,
            reason: Some(PROVISIONAL_COMMAND_ACK_REASON.to_owned()),
        };
        self.store
            .store_command_ack(&scoped_idempotency_key, &provisional_ack)
            .await?;
        let result: Result<CommandAck, ClientError> = async {
            let ack = match command.kind {
                SessionCommandKind::Create => {
                    let session_lease = match self.try_acquire_session_lease(session_id)? {
                        SessionLeaseAttempt::Acquired(lease) => lease,
                        SessionLeaseAttempt::Busy => {
                            return Ok(CommandAck {
                                command_id,
                                accepted: false,
                                reason: Some(
                                    "session is active in another Golutra runtime process"
                                        .to_owned(),
                                ),
                            });
                        }
                    };
                    self.ensure_session_in_workspace(session_id).await?;
                    self.upsert_current_thread(session_id, &command.payload)
                        .await?;
                    self.record_event(host_event(
                        self.next_sequence_no(),
                        session_id,
                        None,
                        RuntimeEventType::SessionCreated,
                        RuntimeEventSource::Runtime,
                        json!({
                            "summary": "runtime host created session",
                            "command_id": command_id.to_string(),
                        }),
                    ))
                    .await?;
                    drop(session_lease);
                    CommandAck {
                        command_id,
                        accepted: true,
                        reason: Some(format!("session {session_id} is ready")),
                    }
                }
                SessionCommandKind::Prompt => {
                    self.clone().handle_prompt(session_id, command).await?
                }
                SessionCommandKind::Abort => {
                    self.handle_lane_command(session_id, &command, "abort")
                        .await?
                }
                SessionCommandKind::Takeover => {
                    self.handle_takeover_command(session_id, &command).await?
                }
                SessionCommandKind::Pause => {
                    self.handle_lane_command(session_id, &command, "pause")
                        .await?
                }
                SessionCommandKind::Resume => {
                    self.handle_lane_command(session_id, &command, "resume")
                        .await?
                }
                SessionCommandKind::Approve => {
                    self.handle_approval_command(session_id, command, ApprovalDecision::Approved)
                        .await?
                }
                SessionCommandKind::Deny => {
                    self.handle_approval_command(session_id, command, ApprovalDecision::Denied)
                        .await?
                }
                SessionCommandKind::Compact => {
                    self.handle_compact_command(session_id, command).await?
                }
                SessionCommandKind::MemoryRollback => {
                    self.handle_memory_rollback_command(session_id, command)
                        .await?
                }
                SessionCommandKind::RunRegression => {
                    self.handle_regression_command(session_id, command).await?
                }
                SessionCommandKind::ApplyCandidate => {
                    self.handle_apply_candidate_command(session_id, command)
                        .await?
                }
                SessionCommandKind::RollbackCandidate => {
                    self.handle_rollback_candidate_command(session_id, command)
                        .await?
                }
                _ => {
                    self.record_event(host_event(
                        self.next_sequence_no(),
                        session_id,
                        None,
                        RuntimeEventType::CommandAccepted,
                        RuntimeEventSource::Runtime,
                        json!({
                            "summary": format!("accepted {:?}", command.kind),
                            "command_id": command_id.to_string(),
                            "payload": command.payload,
                        }),
                    ))
                    .await?;
                    CommandAck {
                        command_id,
                        accepted: true,
                        reason: Some(format!("accepted in session {session_id}")),
                    }
                }
            };
            Ok(ack)
        }
        .await;
        match result {
            Ok(ack) => {
                self.store
                    .store_command_ack(&scoped_idempotency_key, &ack)
                    .await?;
                Ok(ack)
            }
            Err(error) => {
                self.store
                    .store_command_ack(
                        &scoped_idempotency_key,
                        &CommandAck {
                            command_id,
                            accepted: false,
                            reason: Some(error.to_string()),
                        },
                    )
                    .await?;
                Err(error)
            }
        }
    }

    async fn handle_prompt(
        self: Arc<Self>,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let task_id = TaskId::new();
        let turn_id = TurnId::new();
        let payload = command.payload.clone();
        let prompt = prompt_from_payload(&payload);
        if prompt.trim().is_empty() {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some("prompt cannot be empty".to_owned()),
            });
        }
        let busy_decision = {
            let lane_manager = self.lane_manager.lock().await;
            lane_manager
                .lane(session_id)
                .filter(|lane| is_active_status(lane.status))
                .map(|lane| {
                    let task_id = lane.task_id;
                    lane_manager
                        .decide_busy_policy(
                            session_id,
                            command.command_id,
                            &command.actor,
                            BusyPolicy::Append,
                        )
                        .map(|decision| (task_id, decision))
                })
                .transpose()?
        };
        if let Some((active_task_id, decision)) = busy_decision {
            let mut accepted = decision.applied_policy != BusyPolicy::Reject;
            let mut reason = decision.reason.clone();
            let mut retry_as_new_task = false;
            if accepted {
                self.upsert_current_thread(session_id, &payload).await?;
                let control = self.task_controls.lock().await.get(&session_id).cloned();
                match control {
                    Some(control) if control.task_id == active_task_id => {
                        match control
                            .execution
                            .append_turn(PendingAgentTurn {
                                command_id: command.command_id,
                                turn_id,
                                content: prompt.clone(),
                            })
                            .await
                        {
                            Ok(()) => {
                                let transition = self.lane_manager.lock().await.queue_turn(
                                    session_id,
                                    turn_id,
                                    self.next_sequence_no(),
                                )?;
                                self.record_event(with_command_payload(
                                    transition.event,
                                    command.command_id,
                                    payload.clone(),
                                ))
                                .await?;
                            }
                            Err(AgentLoopError::PendingTurnQueueClosed) => {
                                retry_as_new_task = true;
                            }
                            Err(AgentLoopError::PendingTurnQueueFull) => {
                                accepted = false;
                                reason = "active task pending turn queue is full".to_owned();
                            }
                            Err(error) => {
                                return Err(ClientError::TaskExecution(error.to_string()));
                            }
                        }
                    }
                    _ => {
                        retry_as_new_task = true;
                    }
                }
            }
            if retry_as_new_task {
                self.wait_for_finishing_task_control(session_id).await?;
            } else {
                self.record_event(host_event(
                    self.next_sequence_no(),
                    session_id,
                    Some(active_task_id),
                    if accepted {
                        RuntimeEventType::BusyPolicyDecided
                    } else {
                        RuntimeEventType::CommandRejected
                    },
                    RuntimeEventSource::Runtime,
                    json!({
                        "summary": reason,
                        "command_id": command.command_id.to_string(),
                        "decision": decision,
                        "payload": command.payload,
                    }),
                ))
                .await?;
                return Ok(CommandAck {
                    command_id: command.command_id,
                    accepted,
                    reason: Some(if accepted {
                        "prompt appended to active runtime lane".to_owned()
                    } else {
                        "prompt rejected by runtime lane busy policy".to_owned()
                    }),
                });
            }
        }
        self.wait_for_finishing_task_control(session_id).await?;
        let session_lease = match self.try_acquire_session_lease(session_id)? {
            SessionLeaseAttempt::Acquired(lease) => lease,
            SessionLeaseAttempt::Busy => {
                return Ok(CommandAck {
                    command_id: command.command_id,
                    accepted: false,
                    reason: Some("session is active in another Golutra runtime process".to_owned()),
                });
            }
        };
        if let Some(active_task_id) = self.persisted_active_task(session_id).await? {
            self.record_orphaned_task_cancelled(
                session_id,
                Some(active_task_id),
                "session_lease_reacquired",
                "orphaned persisted task cancelled before starting the next prompt",
            )
            .await?;
        }

        self.upsert_current_thread(session_id, &payload).await?;
        let mut lane_manager = self.lane_manager.lock().await;
        let transition = lane_manager.start_task(
            self.workspace_id,
            session_id,
            task_id,
            turn_id,
            command.actor.clone(),
            self.next_sequence_no(),
        )?;
        drop(lane_manager);
        if let Err(error) = self
            .record_event(with_command_payload(
                transition.event,
                command.command_id,
                payload.clone(),
            ))
            .await
        {
            let _ = self.lane_manager.lock().await.finish_task(
                session_id,
                TaskStatus::Failed,
                self.next_sequence_no(),
            );
            return Err(error);
        }
        self.clone()
            .spawn_agent_task(
                HostedAgentTask {
                    session_id,
                    task_id,
                    turn_id,
                    payload,
                },
                session_lease,
                Vec::new(),
            )
            .await?;

        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some(format!("started task {task_id} in session {session_id}")),
        })
    }

    async fn handle_lane_command(
        &self,
        session_id: SessionId,
        command: &SessionCommand,
        action: &str,
    ) -> Result<CommandAck, ClientError> {
        let command_id = command.command_id;
        let task_control = self.task_controls.lock().await.get(&session_id).cloned();
        let Some(task_control) = task_control else {
            let active_task_id = self.persisted_active_task(session_id).await?;
            if action == "abort"
                && let Some(active_task_id) = active_task_id
            {
                let session_lease = match self.try_acquire_session_lease(session_id)? {
                    SessionLeaseAttempt::Acquired(lease) => lease,
                    SessionLeaseAttempt::Busy => {
                        return Ok(CommandAck {
                            command_id,
                            accepted: false,
                            reason: Some(
                                "abort rejected because the active task belongs to another runtime process"
                                    .to_owned(),
                            ),
                        });
                    }
                };
                self.record_orphaned_task_cancelled(
                    session_id,
                    Some(active_task_id),
                    "controller_abort_after_owner_exit",
                    "orphaned persisted task cancelled by controller",
                )
                .await?;
                drop(session_lease);
                return Ok(CommandAck {
                    command_id,
                    accepted: true,
                    reason: Some("orphaned persisted task cancelled".to_owned()),
                });
            }
            return Ok(CommandAck {
                command_id,
                accepted: false,
                reason: Some(active_task_id.map_or_else(
                    || format!("{action} rejected because the session has no active task"),
                    |_| {
                        format!(
                            "{action} rejected because the active task belongs to another runtime process"
                        )
                    },
                )),
            });
        };
        if task_control.abort_handle.is_finished() {
            return Ok(CommandAck {
                command_id,
                accepted: false,
                reason: Some(format!(
                    "{action} rejected because the task already finished"
                )),
            });
        }
        let mut lane_manager = self.lane_manager.lock().await;
        if lane_manager
            .lane(session_id)
            .is_some_and(|lane| lane.active_controller != command.actor)
        {
            return Ok(CommandAck {
                command_id,
                accepted: false,
                reason: Some(format!(
                    "{action} rejected because the actor is not the active controller"
                )),
            });
        }
        let transition = match action {
            "abort" => lane_manager.abort(session_id, self.next_sequence_no()),
            "pause" => lane_manager.pause(session_id, self.next_sequence_no()),
            "resume" => lane_manager.resume(session_id, self.next_sequence_no()),
            _ => unreachable!("lane action is constrained by caller"),
        };
        drop(lane_manager);
        match transition {
            Ok(transition) => {
                self.record_event(with_command_payload(
                    transition.event,
                    command_id,
                    json!({ "action": action }),
                ))
                .await?;
                match action {
                    "abort" => task_control.execution.cancel(),
                    "pause" => task_control.execution.pause(),
                    "resume" => task_control.execution.resume(),
                    _ => unreachable!("lane action is constrained by caller"),
                }
            }
            Err(RuntimeLaneError::LaneNotFound) => {
                return Ok(CommandAck {
                    command_id,
                    accepted: false,
                    reason: Some(format!(
                        "{action} rejected because the runtime lane is not in a compatible active state"
                    )),
                });
            }
            Err(error) => return Err(error.into()),
        }
        Ok(CommandAck {
            command_id,
            accepted: true,
            reason: Some(format!("{action} accepted in session {session_id}")),
        })
    }

    async fn handle_takeover_command(
        &self,
        session_id: SessionId,
        command: &SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        if !self.task_controls.lock().await.contains_key(&session_id) {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some(
                    "takeover rejected because the session has no locally active task".to_owned(),
                ),
            });
        }
        let transition = self.lane_manager.lock().await.takeover(
            session_id,
            command.actor.clone(),
            self.next_sequence_no(),
        );
        match transition {
            Ok(transition) => {
                self.record_event(with_command_payload(
                    transition.event,
                    command.command_id,
                    json!({"action": "takeover"}),
                ))
                .await?;
                Ok(CommandAck {
                    command_id: command.command_id,
                    accepted: true,
                    reason: Some("active runtime controller transferred".to_owned()),
                })
            }
            Err(RuntimeLaneError::LaneNotFound) => Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some("takeover rejected because the session has no active task".to_owned()),
            }),
            Err(error) => Err(error.into()),
        }
    }

    async fn handle_approval_command(
        &self,
        session_id: SessionId,
        command: SessionCommand,
        decision: ApprovalDecision,
    ) -> Result<CommandAck, ClientError> {
        if self
            .lane_manager
            .lock()
            .await
            .lane(session_id)
            .is_some_and(|lane| lane.active_controller != command.actor)
        {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some(
                    "approval rejected because the actor is not the active controller".to_owned(),
                ),
            });
        }
        let state = self.store.query_state(session_id, None).await?;
        let pending_approval = state
            .pending_approval
            .as_deref()
            .and_then(|value| value.parse::<ApprovalId>().ok());
        let requested_approval = command
            .payload
            .get("approval_id")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<ApprovalId>().ok())
            .or(pending_approval);
        let Some(approval_id) = requested_approval else {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some("session has no pending approval".to_owned()),
            });
        };
        if pending_approval != Some(approval_id) {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some(format!(
                    "approval {approval_id} is not pending in this session"
                )),
            });
        }
        let control = self.task_controls.lock().await.get(&session_id).cloned();
        let Some(control) = control else {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some("active task control is unavailable".to_owned()),
            });
        };
        control
            .execution
            .resolve_approval(ApprovalResolution {
                approval_id,
                decision,
                reason: format!("resolved by {}", command.actor.id),
            })
            .await
            .map_err(|error| ClientError::TaskExecution(error.to_string()))?;

        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some(format!("approval {approval_id} resolved as {decision:?}")),
        })
    }

    async fn handle_compact_command(
        &self,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        if self
            .lane_manager
            .lock()
            .await
            .lane(session_id)
            .is_some_and(|lane| lane.active_controller != command.actor)
        {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some(
                    "compaction rejected because the actor is not the active controller".to_owned(),
                ),
            });
        }
        let events = self
            .store
            .load_recent_events(session_id, None, None, MAX_HISTORY_SOURCE_EVENTS)
            .await?;
        let explicit_compaction = self
            .store
            .load_latest_explicit_compaction(session_id)
            .await?
            .as_ref()
            .and_then(explicit_compaction_from_event);
        let compacted_after = explicit_compaction
            .as_ref()
            .map(|(sequence_no, _)| *sequence_no)
            .unwrap_or_default();
        let lines = events
            .iter()
            .filter(|event| event.sequence_no > compacted_after)
            .filter_map(conversation_history_line)
            .collect::<Vec<_>>();
        if explicit_compaction.is_none() && lines.is_empty() {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some("session has no conversation history to compact".to_owned()),
            });
        }
        let summary = compact_history_with_summary(
            explicit_compaction.map(|(_, content)| format!("Summary: {content}")),
            lines,
        );
        let active_task_id = self
            .store
            .query_state(session_id, None)
            .await?
            .active_task_id;
        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            active_task_id,
            RuntimeEventType::CompactionCompleted,
            RuntimeEventSource::Runtime,
            json!({
                "summary": "conversation history compacted",
                "content": summary,
                "command_id": command.command_id,
                "mode": "explicit",
            }),
        ))
        .await?;
        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some("conversation history compacted".to_owned()),
        })
    }

    async fn handle_memory_rollback_command(
        &self,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let memory_id = command
            .payload
            .get("memory_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ClientError::InvalidSession("memory_id is required".to_owned()))?
            .parse::<MemoryId>()
            .map_err(|error| ClientError::InvalidSession(error.to_string()))?;
        let reason = command
            .payload
            .get("reason")
            .and_then(Value::as_str)
            .filter(|reason| !reason.trim().is_empty())
            .unwrap_or("rolled back by user")
            .to_owned();
        let memory_store = self.memory_store.clone();
        let record = run_blocking(move || memory_store.rollback(memory_id, reason)).await??;
        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            None,
            RuntimeEventType::MemoryRolledBack,
            RuntimeEventSource::Memory,
            json!({
                "summary": format!("project memory {memory_id} rolled back"),
                "record": record,
                "command_id": command.command_id,
            }),
        ))
        .await?;
        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some(format!("project memory {memory_id} rolled back")),
        })
    }

    async fn handle_regression_command(
        &self,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let candidate_id = candidate_id_from_payload(&command.payload)?.to_owned();
        let evaluation_store = self.evaluation_store.clone();
        let regression = run_blocking({
            let candidate_id = candidate_id.clone();
            move || evaluation_store.run_regression(&candidate_id)
        })
        .await??;
        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            None,
            RuntimeEventType::RegressionCompleted,
            RuntimeEventSource::Evaluator,
            json!({
                "summary": format!("candidate {candidate_id} regression completed"),
                "record": regression,
                "command_id": command.command_id,
            }),
        ))
        .await?;
        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some(format!("candidate {candidate_id} regression completed")),
        })
    }

    async fn handle_apply_candidate_command(
        &self,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let candidate_id = candidate_id_from_payload(&command.payload)?.to_owned();
        let evaluation_store = self.evaluation_store.clone();
        let decision = run_blocking({
            let candidate_id = candidate_id.clone();
            move || evaluation_store.decide_promotion(&candidate_id)
        })
        .await??;
        let approved = decision.decision == PromotionDecisionKind::Approve;
        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            None,
            RuntimeEventType::PromotionDecided,
            RuntimeEventSource::Evaluator,
            json!({
                "summary": format!("candidate {candidate_id} promotion decision: {:?}", decision.decision),
                "record": decision,
                "command_id": command.command_id,
            }),
        ))
        .await?;
        if !approved {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some(format!(
                    "candidate {candidate_id} did not pass the automatic promotion gate"
                )),
            });
        }
        let evaluation_store = self.evaluation_store.clone();
        let applied = run_blocking({
            let candidate_id = candidate_id.clone();
            move || evaluation_store.apply_candidate(&candidate_id)
        })
        .await??;
        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            None,
            RuntimeEventType::CandidateApplied,
            RuntimeEventSource::Evaluator,
            json!({
                "summary": format!("candidate {candidate_id} applied"),
                "record": applied,
                "command_id": command.command_id,
            }),
        ))
        .await?;
        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some(format!("candidate {candidate_id} applied")),
        })
    }

    async fn handle_rollback_candidate_command(
        &self,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let candidate_id = candidate_id_from_payload(&command.payload)?.to_owned();
        let reason = command
            .payload
            .get("reason")
            .and_then(Value::as_str)
            .filter(|reason| !reason.trim().is_empty())
            .unwrap_or("rolled back by user")
            .to_owned();
        let evaluation_store = self.evaluation_store.clone();
        let rolled_back = run_blocking({
            let candidate_id = candidate_id.clone();
            move || evaluation_store.rollback_candidate(&candidate_id, reason)
        })
        .await??;
        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            None,
            RuntimeEventType::CandidateRolledBack,
            RuntimeEventSource::Evaluator,
            json!({
                "summary": format!("candidate {candidate_id} rolled back"),
                "record": rolled_back,
                "command_id": command.command_id,
            }),
        ))
        .await?;
        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some(format!("candidate {candidate_id} rolled back")),
        })
    }

    async fn query(&self, query: RuntimeQuery) -> Result<Value, ClientError> {
        self.ensure_session_in_workspace(query.session_id).await?;
        let value = match query.kind {
            RuntimeQueryKind::SessionState | RuntimeQueryKind::TaskState => serde_json::to_value(
                self.store
                    .query_state(query.session_id, query.task_id)
                    .await?,
            )?,
            RuntimeQueryKind::UserProjection => serde_json::to_value(
                self.store
                    .user_projection(query.session_id, query.task_id)
                    .await?,
            )?,
            RuntimeQueryKind::DebugProjection => serde_json::to_value(
                self.store
                    .debug_projection(query.session_id, query.task_id)
                    .await?,
            )?,
            RuntimeQueryKind::ReplayCursor => serde_json::to_value(
                self.store
                    .load_events(query.session_id, query.task_id, query.cursor)
                    .await?,
            )?,
            RuntimeQueryKind::MemoryList => {
                let memory_store = self.memory_store.clone();
                serde_json::to_value(run_blocking(move || memory_store.list()).await??)?
            }
            RuntimeQueryKind::EvaluationResults => {
                let evaluation_store = self.evaluation_store.clone();
                let state = run_blocking(move || evaluation_store.snapshot()).await??;
                json!({
                    "cases": state.cases,
                    "runs": state.runs,
                    "results": state.results,
                    "replays": state.replays,
                    "reviews": state.reviews,
                })
            }
            RuntimeQueryKind::ImprovementCandidates => {
                let evaluation_store = self.evaluation_store.clone();
                serde_json::to_value(
                    run_blocking(move || evaluation_store.snapshot())
                        .await??
                        .improvement_candidates,
                )?
            }
            RuntimeQueryKind::AutomationCandidates => {
                let evaluation_store = self.evaluation_store.clone();
                let state = run_blocking(move || evaluation_store.snapshot()).await??;
                json!({
                    "candidates": state.automation_candidates,
                    "generated_tasks": state.generated_tasks,
                    "skill_candidates": state.skill_candidates,
                    "benchmark_promotions": state.benchmark_promotions,
                    "regressions": state.regressions,
                    "promotion_decisions": state.promotion_decisions,
                    "applied_candidates": state.applied_candidates,
                })
            }
        };
        Ok(value)
    }

    async fn replay_events(&self, filter: EventFilter) -> Result<Vec<Value>, ClientError> {
        self.ensure_session_in_workspace(filter.session_id).await?;
        let events = self
            .store
            .load_events(filter.session_id, filter.task_id, filter.after_sequence_no)
            .await?;
        events
            .into_iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()
            .map_err(ClientError::Serialization)
    }

    pub async fn list_threads(&self, limit: u32) -> Result<Vec<ThreadRecord>, ClientError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let workspace_root = self.workspace_root_string();
        let threads = self
            .store
            .list_threads(workspace_root.as_deref(), limit)
            .await?
            .into_iter()
            .take(limit as usize)
            .collect();
        Ok(threads)
    }

    pub async fn thread_for_session(
        &self,
        session_id: SessionId,
    ) -> Result<Option<ThreadRecord>, ClientError> {
        let thread = self.store.thread_by_session(session_id).await?;
        if let Some(thread) = &thread {
            self.ensure_thread_in_workspace(thread)?;
        }
        Ok(thread)
    }

    pub async fn resume_thread(&self, thread_id: ThreadId) -> Result<ThreadRecord, ClientError> {
        let thread = self.store.thread_by_id(thread_id).await?.ok_or_else(|| {
            ClientError::InvalidSession(format!("thread `{thread_id}` not found"))
        })?;
        self.ensure_thread_in_workspace(&thread)?;
        Ok(thread)
    }

    pub async fn fork_thread(
        &self,
        thread_id: ThreadId,
        from_turn_id: Option<TurnId>,
    ) -> Result<ThreadRecord, ClientError> {
        let parent = self.store.thread_by_id(thread_id).await?.ok_or_else(|| {
            ClientError::InvalidSession(format!("thread `{thread_id}` not found"))
        })?;
        self.ensure_thread_in_workspace(&parent)?;
        let parent_state = self.store.query_state(parent.session_id, None).await?;
        if is_active_status(parent_state.task_status) {
            return Err(ClientError::InvalidSession(format!(
                "thread `{thread_id}` cannot be forked while its task is active"
            )));
        }
        let parent_events = self
            .store
            .load_events(parent.session_id, None, None)
            .await?;
        let through_sequence_no = match from_turn_id {
            Some(turn_id) => parent_events
                .iter()
                .filter(|event| event.turn_id == Some(turn_id))
                .map(|event| event.sequence_no)
                .max()
                .ok_or_else(|| {
                    ClientError::InvalidSession(format!(
                        "turn `{turn_id}` was not found in thread `{thread_id}`"
                    ))
                })?,
            None => parent_events
                .last()
                .map(|event| event.sequence_no)
                .unwrap_or_default(),
        };
        let now = chrono::Utc::now();
        let child_thread_id = ThreadId::new();
        let child_session_id = SessionId::new();
        let child = ThreadRecord {
            thread_id: child_thread_id,
            session_id: child_session_id,
            parent_thread_id: Some(parent.thread_id),
            forked_from_turn_id: from_turn_id,
            forked_from_sequence_no: Some(through_sequence_no),
            workspace_root: parent.workspace_root.clone(),
            rebound_from_workspace_root: None,
            rollout_path: self
                .runtime_paths
                .as_ref()
                .map(|paths| paths.rollout_path(child_thread_id).display().to_string()),
            title: format!("Fork of {}", parent.title),
            preview: parent.preview.clone(),
            created_at: now,
            updated_at: now,
            recency_at: now,
            archived: false,
        };
        let _writer = self.event_writer.lock().await;
        let forked_events = self
            .store
            .create_forked_thread(&child, parent.session_id, through_sequence_no)
            .await?;
        for event in &forked_events {
            let _ = self.event_bus.send(event.clone());
        }
        drop(_writer);
        let child_state = self.store.query_state(child.session_id, None).await?;
        if is_active_status(child_state.task_status) {
            self.record_event(host_event(
                self.next_sequence_no(),
                child.session_id,
                child_state.active_task_id,
                RuntimeEventType::TaskCompleted,
                RuntimeEventSource::Runtime,
                json!({
                    "summary": "fork history closed at the selected turn boundary",
                    "status": TaskStatus::Completed,
                    "fork_boundary": true,
                }),
            ))
            .await?;
        }
        self.record_event(host_event(
            self.next_sequence_no(),
            child.session_id,
            None,
            RuntimeEventType::ThreadForked,
            RuntimeEventSource::Runtime,
            json!({
                "summary": format!("thread forked from {}", parent.thread_id),
                "parent_thread_id": parent.thread_id,
                "forked_from_turn_id": from_turn_id,
                "forked_from_sequence_no": through_sequence_no,
            }),
        ))
        .await?;
        self.rebuild_thread_rollout(&child).await?;
        Ok(child)
    }

    pub async fn export_thread_rollout(
        &self,
        thread_id: ThreadId,
    ) -> Result<RolloutExport, ClientError> {
        let mut thread = self.store.thread_by_id(thread_id).await?.ok_or_else(|| {
            ClientError::InvalidSession(format!("thread `{thread_id}` not found"))
        })?;
        self.ensure_thread_in_workspace(&thread)?;
        self.ensure_thread_rollout_path(&mut thread).await?;
        self.rebuild_thread_rollout(&thread).await
    }

    pub async fn rebind_thread(
        &self,
        thread_id: ThreadId,
        from_workspace_root: impl AsRef<Path>,
    ) -> Result<ThreadRebindResult, ClientError> {
        let new_workspace_root = self.workspace_root_string().ok_or_else(|| {
            ClientError::InvalidSession("thread rebind requires a cwd runtime".to_owned())
        })?;
        let source_workspace_root = normalize_rebind_source(from_workspace_root.as_ref())?;
        let from_workspace_root = source_workspace_root.display().to_string();
        let mut thread = self.store.thread_by_id(thread_id).await?.ok_or_else(|| {
            ClientError::InvalidSession(format!("thread `{thread_id}` not found"))
        })?;
        if thread.workspace_root.as_deref() != Some(from_workspace_root.as_str()) {
            return Err(ClientError::InvalidSession(format!(
                "thread `{thread_id}` belongs to `{}`, not `{from_workspace_root}`",
                thread.workspace_root.as_deref().unwrap_or("<none>")
            )));
        }
        let state = self.store.query_state(thread.session_id, None).await?;
        if is_active_status(state.task_status) {
            return Err(ClientError::InvalidSession(format!(
                "thread `{thread_id}` cannot be rebound while its task is active"
            )));
        }
        let SessionLeaseAttempt::Acquired(_lease) =
            self.try_acquire_session_lease(thread.session_id)?
        else {
            return Err(ClientError::InvalidSession(format!(
                "thread `{thread_id}` is owned by another runtime"
            )));
        };
        let expected_old_rollout_path = self.runtime_paths.as_ref().map(|paths| {
            rollout_path_for_workspace(paths, &source_workspace_root, thread.thread_id)
        });
        let old_rollout_path = match (&thread.rollout_path, expected_old_rollout_path) {
            (Some(configured), Some(expected)) if Path::new(configured) == expected => {
                Some(expected)
            }
            (Some(configured), Some(expected)) => {
                return Err(ClientError::InvalidSession(format!(
                    "thread `{thread_id}` rollout path `{configured}` does not match source workspace path `{}`",
                    expected.display()
                )));
            }
            (Some(_), None) => {
                return Err(ClientError::InvalidSession(
                    "thread rebind requires durable runtime paths".to_owned(),
                ));
            }
            (None, _) => None,
        };
        thread.workspace_root = Some(new_workspace_root.clone());
        thread.rebound_from_workspace_root = Some(from_workspace_root.clone());
        thread.rollout_path = self
            .runtime_paths
            .as_ref()
            .map(|paths| paths.rollout_path(thread.thread_id).display().to_string());
        thread.updated_at = chrono::Utc::now();
        thread.recency_at = thread.updated_at;
        self.store.upsert_thread(&thread).await?;
        let rollout = self.rebuild_thread_rollout(&thread).await?;
        self.record_event(host_event(
            self.next_sequence_no(),
            thread.session_id,
            state.active_task_id,
            RuntimeEventType::ThreadRebound,
            RuntimeEventSource::Runtime,
            json!({
                "summary": format!("thread rebound from {from_workspace_root} to {new_workspace_root}"),
                "thread_id": thread.thread_id,
                "from_workspace_root": from_workspace_root,
                "to_workspace_root": new_workspace_root,
                "checkpoint_compatibility": "historical_only",
            }),
        ))
        .await?;
        if let Some(old_path) = old_rollout_path
            && thread.rollout_path.as_deref() != Some(old_path.to_string_lossy().as_ref())
            && old_path.exists()
        {
            fs::remove_file(&old_path)
                .map_err(|error| ClientError::Io(format!("{}: {error}", old_path.display())))?;
        }
        Ok(ThreadRebindResult {
            thread,
            previous_workspace_root: from_workspace_root,
            rollout_rebuilt: rollout.event_count > 0,
            checkpoint_compatibility: "historical_only".to_owned(),
        })
    }

    async fn upsert_current_thread(
        &self,
        session_id: SessionId,
        payload: &Value,
    ) -> Result<(), ClientError> {
        let now = chrono::Utc::now();
        let existing = self.store.thread_by_session(session_id).await?;
        if let Some(existing) = &existing {
            self.ensure_thread_in_workspace(existing)?;
        }
        let payload_thread_id = thread_id_from_payload(payload);
        if let (Some(existing), Some(payload_thread_id)) = (&existing, payload_thread_id)
            && existing.thread_id != payload_thread_id
        {
            return Err(ClientError::InvalidSession(format!(
                "session `{session_id}` already belongs to thread `{}`",
                existing.thread_id
            )));
        }
        let payload_thread = match payload_thread_id {
            Some(thread_id) => self.store.thread_by_id(thread_id).await?,
            None => None,
        };
        if let Some(payload_thread) = &payload_thread {
            self.ensure_thread_in_workspace(payload_thread)?;
        }
        if let Some(payload_thread) = &payload_thread
            && payload_thread.session_id != session_id
        {
            return Err(ClientError::InvalidSession(format!(
                "thread `{}` belongs to another session",
                payload_thread.thread_id
            )));
        }
        let source_thread = existing.as_ref().or(payload_thread.as_ref());
        let thread_id = source_thread
            .map(|thread| thread.thread_id)
            .or(payload_thread_id)
            .unwrap_or_else(|| {
                if session_id == self.default_session_id {
                    self.default_thread_id
                } else {
                    ThreadId::new()
                }
            });
        let thread = ThreadRecord {
            thread_id,
            session_id,
            parent_thread_id: source_thread.and_then(|thread| thread.parent_thread_id),
            forked_from_turn_id: source_thread.and_then(|thread| thread.forked_from_turn_id),
            forked_from_sequence_no: source_thread
                .and_then(|thread| thread.forked_from_sequence_no),
            workspace_root: self.workspace_root_string(),
            rebound_from_workspace_root: source_thread
                .and_then(|thread| thread.rebound_from_workspace_root.clone()),
            rollout_path: source_thread
                .and_then(|thread| thread.rollout_path.clone())
                .or_else(|| {
                    self.runtime_paths
                        .as_ref()
                        .map(|paths| paths.rollout_path(thread_id).display().to_string())
                }),
            title: thread_title_for_prompt(source_thread, payload),
            preview: preview_from_payload(payload),
            created_at: source_thread.map(|thread| thread.created_at).unwrap_or(now),
            updated_at: now,
            recency_at: now,
            archived: false,
        };
        self.store.upsert_thread(&thread).await?;
        Ok(())
    }

    async fn record_event(&self, event: RuntimeEvent) -> Result<(), ClientError> {
        let _writer = self.event_writer.lock().await;
        let event = self.store.append_event_assigning_sequence(event).await?;
        self.append_rollout_event(&event).await?;
        let _ = self.event_bus.send(event);
        Ok(())
    }

    async fn synchronize_workspace_rollouts(&self) -> Result<(), ClientError> {
        let Some(workspace_root) = self.workspace_root_string() else {
            return Ok(());
        };
        let threads = self
            .store
            .list_threads(Some(&workspace_root), u32::MAX)
            .await?;
        for mut thread in threads {
            self.ensure_thread_rollout_path(&mut thread).await?;
            self.rebuild_thread_rollout(&thread).await?;
        }
        Ok(())
    }

    async fn ensure_thread_rollout_path(
        &self,
        thread: &mut ThreadRecord,
    ) -> Result<(), ClientError> {
        let Some(paths) = &self.runtime_paths else {
            thread.rollout_path = None;
            return Ok(());
        };
        let expected = paths.rollout_path(thread.thread_id).display().to_string();
        if thread.rollout_path.as_deref() != Some(expected.as_str()) {
            thread.rollout_path = Some(expected);
            thread.updated_at = chrono::Utc::now();
            self.store.upsert_thread(thread).await?;
        }
        Ok(())
    }

    async fn append_rollout_event(&self, event: &RuntimeEvent) -> Result<(), ClientError> {
        let Some(mut thread) = self.store.thread_by_session(event.session_id).await? else {
            return Ok(());
        };
        self.ensure_thread_rollout_path(&mut thread).await?;
        let Some(path) = thread.rollout_path.as_deref().map(PathBuf::from) else {
            return Ok(());
        };
        if !path.exists() {
            self.rebuild_thread_rollout(&thread).await?;
            return Ok(());
        }
        let line = rollout_line(&thread, event)?;
        run_blocking(move || append_rollout_line(&path, &line)).await??;
        Ok(())
    }

    async fn rebuild_thread_rollout(
        &self,
        thread: &ThreadRecord,
    ) -> Result<RolloutExport, ClientError> {
        let Some(path) = thread.rollout_path.as_deref().map(PathBuf::from) else {
            return Ok(RolloutExport {
                thread_id: thread.thread_id,
                session_id: thread.session_id,
                path: String::new(),
                event_count: 0,
                last_sequence_no: None,
            });
        };
        let events = self
            .store
            .load_events(thread.session_id, None, None)
            .await?;
        let lines = events
            .iter()
            .map(|event| rollout_line(thread, event))
            .collect::<Result<Vec<_>, _>>()?;
        let last_sequence_no = events.last().map(|event| event.sequence_no);
        let event_count = events.len();
        let export_path = path.display().to_string();
        run_blocking(move || rebuild_rollout_file(&path, &lines)).await??;
        Ok(RolloutExport {
            thread_id: thread.thread_id,
            session_id: thread.session_id,
            path: export_path,
            event_count,
            last_sequence_no,
        })
    }

    async fn persisted_active_task(
        &self,
        session_id: SessionId,
    ) -> Result<Option<TaskId>, ClientError> {
        let state = self.store.query_state(session_id, None).await?;
        if is_active_status(state.task_status) {
            Ok(state.active_task_id)
        } else {
            Ok(None)
        }
    }

    async fn wait_for_finishing_task_control(
        &self,
        session_id: SessionId,
    ) -> Result<(), ClientError> {
        let mut completion = self
            .task_controls
            .lock()
            .await
            .get(&session_id)
            .map(|control| control.completion.clone());
        let Some(completion) = completion.as_mut() else {
            return Ok(());
        };
        if !*completion.borrow() {
            completion.changed().await.map_err(|_| {
                ClientError::TaskExecution(
                    "previous task supervisor stopped before releasing the session".to_owned(),
                )
            })?;
        }
        Ok(())
    }

    async fn acquire_command_lease(
        &self,
        idempotency_key: &str,
    ) -> Result<Option<File>, ClientError> {
        let Some(paths) = &self.runtime_paths else {
            return Ok(None);
        };
        let path = paths.command_lock(idempotency_key);
        run_blocking(move || {
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&path)
                .map_err(|error| ClientError::Io(format!("{}: {error}", path.display())))?;
            set_owner_only_file(&path)?;
            file.lock_exclusive()
                .map_err(|error| ClientError::Io(format!("{}: {error}", path.display())))?;
            Ok(file)
        })
        .await?
        .map(Some)
    }

    fn try_acquire_session_lease(
        &self,
        session_id: SessionId,
    ) -> Result<SessionLeaseAttempt, ClientError> {
        let Some(paths) = &self.runtime_paths else {
            return Ok(SessionLeaseAttempt::Acquired(None));
        };
        let path = paths.session_lock(session_id);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| ClientError::Io(format!("{}: {error}", path.display())))?;
        set_owner_only_file(&path)?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(SessionLeaseAttempt::Acquired(Some(Arc::new(file)))),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                Ok(SessionLeaseAttempt::Busy)
            }
            Err(error) => Err(ClientError::Io(format!("{}: {error}", path.display()))),
        }
    }

    async fn ensure_session_in_workspace(&self, session_id: SessionId) -> Result<(), ClientError> {
        if let Some(thread) = self.store.thread_by_session(session_id).await? {
            self.ensure_thread_in_workspace(&thread)?;
        }
        Ok(())
    }

    async fn context_contributors_for_task(
        &self,
        session_id: SessionId,
        current_task_id: TaskId,
        objective: String,
    ) -> Result<Vec<ContextContributor>, ClientError> {
        let workspace_root = self.execution_workspace_root()?;
        let mut contributors = vec![ContextContributor {
            name: "system".to_owned(),
            role: ProviderRole::System,
            content: system_prompt(),
            token_budget_hint: 64,
        }];
        contributors.push(ContextContributor {
            name: "environment_context".to_owned(),
            role: ProviderRole::System,
            content: environment_context_prompt(&workspace_root),
            token_budget_hint: 128,
        });
        if let Some(project_instructions) = load_project_instructions(&workspace_root).await? {
            contributors.push(ContextContributor {
                name: "project_instructions".to_owned(),
                role: ProviderRole::System,
                content: project_instructions,
                token_budget_hint: 2_048,
            });
        }

        let memory_store = self.memory_store.clone();
        let memory_query = objective.clone();
        let memories =
            run_blocking(move || memory_store.retrieve(&memory_query, "project", 5)).await??;
        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            Some(current_task_id),
            RuntimeEventType::MemoryRetrieved,
            RuntimeEventSource::Memory,
            json!({
                "summary": format!("retrieved {} project memories", memories.len()),
                "query": objective,
                "scope": "project",
                "retrieved": memories,
            }),
        ))
        .await?;
        if !memories.is_empty() {
            contributors.push(ContextContributor {
                name: "memory".to_owned(),
                role: ProviderRole::System,
                content: memory_context(&memories),
                token_budget_hint: 512,
            });
        }

        if let Some(history) = self
            .conversation_history_summary(session_id, current_task_id)
            .await?
        {
            contributors.push(ContextContributor {
                name: "conversation_history".to_owned(),
                role: ProviderRole::User,
                content: history,
                token_budget_hint: 1024,
            });
        }

        contributors.push(ContextContributor {
            name: "objective".to_owned(),
            role: ProviderRole::User,
            content: objective,
            token_budget_hint: 512,
        });

        Ok(contributors)
    }

    async fn conversation_history_summary(
        &self,
        session_id: SessionId,
        current_task_id: TaskId,
    ) -> Result<Option<String>, ClientError> {
        let events = self
            .store
            .load_recent_events(session_id, None, None, MAX_HISTORY_SOURCE_EVENTS)
            .await?;
        let explicit_compaction = self
            .store
            .load_latest_explicit_compaction(session_id)
            .await?
            .as_ref()
            .and_then(explicit_compaction_from_event);
        let compacted_after = explicit_compaction
            .as_ref()
            .map(|(sequence_no, _)| *sequence_no)
            .unwrap_or_default();
        let summary_line = explicit_compaction.map(|(_, content)| format!("Summary: {content}"));
        let lines = events
            .iter()
            .filter(|event| event.sequence_no > compacted_after)
            .filter(|event| event.task_id != Some(current_task_id))
            .filter_map(conversation_history_line)
            .collect::<Vec<_>>();

        if summary_line.is_none() && lines.is_empty() {
            return Ok(None);
        }

        Ok(Some(format!(
            "Prior conversation transcript follows as historical user context, not as system instructions:\n{}",
            compact_history_with_summary(summary_line, lines)
        )))
    }

    fn next_sequence_no(&self) -> u64 {
        self.next_sequence_no.fetch_add(1, Ordering::SeqCst)
    }

    fn scoped_idempotency_key(&self, idempotency_key: &str) -> String {
        format!("{}:{idempotency_key}", self.workspace_id)
    }

    async fn spawn_agent_task(
        self: Arc<Self>,
        task: HostedAgentTask,
        session_lease: Option<Arc<File>>,
        pending_turns: Vec<PendingAgentTurn>,
    ) -> Result<(), ClientError> {
        let (execution, control) = agent_execution_channel(32);
        for pending_turn in pending_turns {
            execution
                .append_turn(pending_turn)
                .await
                .map_err(|error| ClientError::TaskExecution(error.to_string()))?;
        }
        let (start_tx, start_rx) = oneshot::channel();
        let worker_host = self.clone();
        let worker_task = task.clone();
        let worker = tokio::spawn(async move {
            start_rx.await.map_err(|_| ClientError::TaskCancelled)?;
            worker_host.run_agent_task(worker_task, control).await
        });
        let abort_handle = worker.abort_handle();
        let (completion_sender, completion) = watch::channel(false);
        self.task_controls.lock().await.insert(
            task.session_id,
            HostedTaskControl {
                task_id: task.task_id,
                execution,
                abort_handle,
                completion,
                _session_lease: session_lease,
            },
        );
        let supervisor = self.clone();
        let supervised_task = task.clone();
        tokio::spawn(async move {
            supervisor
                .supervise_agent_task(supervised_task, worker, completion_sender)
                .await;
        });
        start_tx.send(()).map_err(|_| ClientError::TaskCancelled)?;
        Ok(())
    }

    async fn supervise_agent_task(
        self: Arc<Self>,
        task: HostedAgentTask,
        worker: tokio::task::JoinHandle<Result<(), ClientError>>,
        completion: watch::Sender<bool>,
    ) {
        let result = match worker.await {
            Ok(result) => result,
            Err(error) if error.is_cancelled() => Err(ClientError::TaskCancelled),
            Err(error) if error.is_panic() => Err(ClientError::TaskExecution(
                "agent task worker panicked".to_owned(),
            )),
            Err(error) => Err(ClientError::TaskExecution(format!(
                "agent task worker stopped unexpectedly: {error}"
            ))),
        };
        if let Err(error) = result {
            let terminal_status = if matches!(&error, ClientError::TaskCancelled) {
                TaskStatus::Cancelled
            } else {
                TaskStatus::Failed
            };
            if self
                .record_task_execution_failure(&task, error)
                .await
                .is_err()
            {
                let _ = self.finish_lane(&task, terminal_status).await;
            }
        }
        self.clear_task_control(task.session_id, task.task_id).await;
        completion.send_replace(true);
    }

    async fn clear_task_control(&self, session_id: SessionId, task_id: TaskId) {
        let mut controls = self.task_controls.lock().await;
        if controls
            .get(&session_id)
            .is_some_and(|control| control.task_id == task_id)
        {
            controls.remove(&session_id);
        }
    }

    async fn run_agent_task(
        self: Arc<Self>,
        task: HostedAgentTask,
        control: AgentExecutionControl,
    ) -> Result<(), ClientError> {
        let started_at = Instant::now();
        let objective = prompt_from_payload(&task.payload);
        let workspace_root = self.execution_workspace_root()?;
        let policy = WorkspacePolicy::new(workspace_root.clone())
            .map_err(|error| ClientError::TaskExecution(error.to_string()))?;
        let tool_executor = BasicToolExecutor::new(policy);
        let workspace_tool_names = tool_executor
            .registry()
            .contracts()
            .into_iter()
            .map(|contract| contract.tool_name.clone())
            .collect::<Vec<_>>();
        let provider_plan =
            mock_provider_plan(self.runtime_paths.as_ref(), &task.payload, &objective)
                .map_err(|error| ClientError::TaskExecution(error.to_string()))?;
        let MockProviderPlan {
            provider,
            fallback_provider,
            touched_code,
            workspace_tools_enabled,
            context_builder,
        } = provider_plan;
        let agent_loop = AgentLoop::new(provider, context_builder, tool_executor);
        let agent_loop = match fallback_provider {
            Some(fallback) => agent_loop.with_fallback(fallback),
            None => agent_loop,
        };
        let contributors = self
            .context_contributors_for_task(task.session_id, task.task_id, objective.clone())
            .await?;
        let (trace_tx, mut trace_rx) = mpsc::unbounded_channel::<HostedTraceCommand>();
        let trace_host = self.clone();
        let trace_task = task.clone();
        let trace_recorder = tokio::spawn(async move {
            while let Some(command) = trace_rx.recv().await {
                match command {
                    HostedTraceCommand::Event(event) => {
                        trace_host.record_trace_event(&trace_task, *event).await?;
                    }
                    HostedTraceCommand::Flush(sender) => {
                        let _ = sender.send(Ok(()));
                    }
                }
            }
            Ok::<(), ClientError>(())
        });
        let agent_loop = if self.workspace_root.is_some() {
            agent_loop.with_before_side_effect_recorder(Arc::new(HostedCheckpointRecorder {
                host: self.clone(),
                task: task.clone(),
                trace_sender: trace_tx.clone(),
            }))
        } else {
            agent_loop
        };
        let trace_sender = trace_tx.clone();
        let outcome = agent_loop
            .run_with_control_and_trace(
                AgentTaskRequest {
                    session_id: task.session_id,
                    task_id: task.task_id,
                    turn_id: task.turn_id,
                    objective: objective.clone(),
                    completion_criteria: vec![
                        "runtime task produces durable evidence or terminal verification"
                            .to_owned(),
                    ],
                    touched_code,
                    contributors,
                    tools: if workspace_tools_enabled {
                        workspace_tool_names
                    } else {
                        Vec::new()
                    },
                },
                control,
                move |event| {
                    let _ = trace_sender.send(HostedTraceCommand::Event(Box::new(event)));
                },
            )
            .await
            .map_err(|error| match error {
                AgentLoopError::Cancelled => ClientError::TaskCancelled,
                error => ClientError::TaskExecution(error.to_string()),
            });
        drop(agent_loop);
        drop(trace_tx);
        trace_recorder
            .await
            .map_err(|error| ClientError::TaskExecution(error.to_string()))??;
        let outcome = outcome?;
        self.record_event(agent_event_for_turn(
            self.next_sequence_no(),
            &task,
            outcome.final_turn_id,
            RuntimeEventType::VerificationCompleted,
            RuntimeEventSource::Verifier,
            json!({
                "summary": format!("verification result: {:?}", outcome.verification.result),
                "record": outcome.verification,
            }),
        ))
        .await?;
        let terminal_status = task_status_from_loop_action(outcome.loop_decision.action);
        self.record_event(agent_event_for_turn(
            self.next_sequence_no(),
            &task,
            outcome.final_turn_id,
            RuntimeEventType::LoopDecided,
            RuntimeEventSource::Runtime,
            json!({
                "summary": outcome.loop_decision.reason,
                "record": outcome.loop_decision,
            }),
        ))
        .await?;
        let final_task = HostedAgentTask {
            turn_id: outcome.final_turn_id,
            ..task.clone()
        };
        let final_objective = outcome.verification.objective.clone();
        self.promote_successful_task_memory(
            &final_task,
            &final_objective,
            &outcome,
            terminal_status,
        )
        .await?;
        self.evaluate_completed_task(
            &final_task,
            HostedTaskEvaluation {
                objective: &final_objective,
                task_status: terminal_status,
                verification: Some(outcome.verification.clone()),
                tool_reports: &outcome.tool_reports,
                failure_summary: Some(outcome.loop_decision.reason.clone()),
                latency: started_at.elapsed(),
            },
        )
        .await?;
        self.finish_lane(&final_task, terminal_status).await
    }

    async fn evaluate_completed_task(
        &self,
        task: &HostedAgentTask,
        input: HostedTaskEvaluation<'_>,
    ) -> Result<(), ClientError> {
        let events = self
            .store
            .load_events(task.session_id, Some(task.task_id), None)
            .await?;
        let artifact_count = input
            .tool_reports
            .iter()
            .map(|report| report.artifacts.len())
            .sum();
        let bundle = EvaluationRunner.evaluate_task(TaskEvaluationInput {
            task_id: task.task_id,
            objective: input.objective.to_owned(),
            task_status: input.task_status,
            verification: input.verification,
            event_count: events.len(),
            artifact_count,
            tool_count: input.tool_reports.len(),
            latency_ms: Some(u64::try_from(input.latency.as_millis()).unwrap_or(u64::MAX)),
            failure_summary: input.failure_summary,
        });
        self.record_task_evaluation(task, bundle).await
    }

    async fn record_task_evaluation(
        &self,
        task: &HostedAgentTask,
        bundle: TaskEvaluationBundle,
    ) -> Result<(), ClientError> {
        let result = bundle.result.clone();
        let review = bundle.review.clone();
        let improvement_candidate = bundle.improvement_candidate.clone();
        let automation_candidates = bundle.automation_candidates.clone();
        let evaluation_store = self.evaluation_store.clone();
        let persistence =
            run_blocking(move || evaluation_store.record_task_evaluation(bundle)).await?;
        if let Err(error) = persistence {
            self.record_event(agent_event(
                self.next_sequence_no(),
                task,
                RuntimeEventType::EvaluationCompleted,
                RuntimeEventSource::Evaluator,
                json!({
                    "summary": "durable task evaluation failed",
                    "error": error.to_string(),
                }),
            ))
            .await?;
            return Ok(());
        }
        self.record_event(agent_event(
            self.next_sequence_no(),
            task,
            RuntimeEventType::PostTaskReviewed,
            RuntimeEventSource::Evaluator,
            json!({
                "summary": format!("deep post-task review outcome: {}", review.outcome),
                "record": review,
            }),
        ))
        .await?;
        self.record_event(agent_event(
            self.next_sequence_no(),
            task,
            RuntimeEventType::EvaluationCompleted,
            RuntimeEventSource::Evaluator,
            json!({
                "summary": format!("task evaluation verdict: {:?}", result.verdict),
                "record": result,
            }),
        ))
        .await?;
        if let Some(candidate) = improvement_candidate {
            self.record_event(agent_event(
                self.next_sequence_no(),
                task,
                RuntimeEventType::ImprovementCandidateCreated,
                RuntimeEventSource::Evaluator,
                json!({
                    "summary": format!("improvement candidate {} proposed", candidate.id),
                    "record": candidate,
                }),
            ))
            .await?;
        }
        if !automation_candidates.is_empty() {
            self.record_event(agent_event(
                self.next_sequence_no(),
                task,
                RuntimeEventType::AutomationCandidateCreated,
                RuntimeEventSource::Evaluator,
                json!({
                    "summary": format!("{} governed automation candidates proposed", automation_candidates.len()),
                    "records": automation_candidates,
                }),
            ))
            .await?;
        }
        Ok(())
    }

    async fn promote_successful_task_memory(
        &self,
        task: &HostedAgentTask,
        objective: &str,
        outcome: &golutra_runtime::AgentLoopOutcome,
        terminal_status: TaskStatus,
    ) -> Result<(), ClientError> {
        if terminal_status != TaskStatus::Completed || outcome.verification.evidence_refs.is_empty()
        {
            return Ok(());
        }
        let tool_facts = outcome
            .tool_reports
            .iter()
            .map(|report| report.envelope.summary.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        let final_message = outcome
            .final_message
            .as_deref()
            .unwrap_or("verified completion");
        let content = format!(
            "Objective: {}\nVerified outcome: {}\nEvidence-backed facts: {}",
            compact_history_text(objective, 320),
            compact_history_text(final_message, 480),
            compact_history_text(&tool_facts, 480),
        );
        let candidate =
            propose_project_memory(task.task_id, outcome.verification.evidence_refs.clone());
        let memory_store = self.memory_store.clone();
        let promotion = run_blocking(move || {
            memory_store.promote(&MemoryPromotionGate::default(), &candidate, content)
        })
        .await?;
        match promotion {
            Ok(record) => {
                self.record_event(agent_event(
                    self.next_sequence_no(),
                    task,
                    RuntimeEventType::MemoryPromoted,
                    RuntimeEventSource::Memory,
                    json!({
                        "summary": format!("project memory {} promoted", record.memory_id),
                        "record": record,
                    }),
                ))
                .await?;
            }
            Err(error) => {
                self.record_event(agent_event(
                    self.next_sequence_no(),
                    task,
                    RuntimeEventType::MemoryPromotionRejected,
                    RuntimeEventSource::Memory,
                    json!({
                        "summary": "project memory promotion rejected",
                        "reason": error.to_string(),
                    }),
                ))
                .await?;
            }
        }
        Ok(())
    }

    async fn record_trace_event(
        &self,
        task: &HostedAgentTask,
        trace_event: AgentLoopTraceEvent,
    ) -> Result<(), ClientError> {
        if let AgentLoopTraceEvent::ToolCompleted(report) = &trace_event {
            return self.record_tool_report(task, report).await;
        }
        if let AgentLoopTraceEvent::PendingTurnStarted(turn) = &trace_event {
            self.lane_manager
                .lock()
                .await
                .start_queued_turn(task.session_id, turn.turn_id)?;
        }
        let active_turn_id = self
            .lane_manager
            .lock()
            .await
            .lane(task.session_id)
            .and_then(|lane| lane.active_turn_id)
            .unwrap_or(task.turn_id);
        let event_turn_id = match &trace_event {
            AgentLoopTraceEvent::PendingTurnStarted(turn) => Some(turn.turn_id),
            AgentLoopTraceEvent::AssistantMessage { turn_id, .. } => Some(*turn_id),
            AgentLoopTraceEvent::ApprovalRequested(approval) => Some(approval.turn_id),
            AgentLoopTraceEvent::TokenUsageRecorded(record) => Some(record.turn_id),
            _ => Some(active_turn_id),
        };
        let raw_artifact = match &trace_event {
            AgentLoopTraceEvent::ProviderCompleted { raw_metadata, .. } => {
                Some(provider_raw_artifact(task, active_turn_id, raw_metadata)?)
            }
            _ => None,
        };
        if let Some((event_type, source, payload)) = trace_event_payload(trace_event) {
            let mut event = agent_event(self.next_sequence_no(), task, event_type, source, payload);
            if let Some(turn_id) = event_turn_id {
                event.turn_id = Some(turn_id);
            }
            if let Some((mut artifact, bytes)) = raw_artifact {
                artifact.provenance_refs.push(event.id);
                self.store.store_artifact(&artifact, &bytes).await?;
                event.payload_ref = Some(artifact.artifact_id);
                event.payload["raw_metadata_ref"] = Value::String(artifact.artifact_id.to_string());
            }
            self.record_event(event).await?;
        }
        Ok(())
    }

    async fn persist_checkpoint_before_side_effect(
        &self,
        task: &HostedAgentTask,
        request: &ToolRequest,
        before_images: &[FileBeforeImage],
    ) -> Result<(), ClientError> {
        let workspace_root = self.execution_workspace_root()?;
        let checkpoint_root = self
            .runtime_paths
            .as_ref()
            .map(|paths| paths.checkpoints_dir.clone())
            .ok_or_else(|| {
                ClientError::TaskExecution(
                    "durable checkpoint path is unavailable for this runtime".to_owned(),
                )
            })?;
        let manager = WorkspaceCheckpointManager::new(workspace_root.clone(), checkpoint_root);
        let workspace_id = self.workspace_id;
        let task_id = task.task_id;
        let turn_id = request.turn_id.unwrap_or(task.turn_id);
        let tool_call_id = request.tool_call_id;
        let owned_before_images = before_images.to_vec();
        let mut checkpoint = run_blocking(move || {
            manager.create_checkpoint(
                workspace_id,
                task_id,
                turn_id,
                &owned_before_images,
                tool_call_id,
            )
        })
        .await?
        .map_err(|error| ClientError::TaskExecution(error.to_string()))?;

        let checkpoint_event_id = EventId::new();
        for before_image in before_images {
            let Some(bytes) = &before_image.content else {
                continue;
            };
            let artifact_id = ArtifactId::new();
            let checksum = Sha256::digest(bytes);
            let artifact = ArtifactRecord {
                artifact_id,
                session_id: task.session_id,
                turn_id: request.turn_id,
                tool_call_id: Some(tool_call_id),
                artifact_type: "checkpoint_before_image".to_owned(),
                uri: format!(
                    "artifact://checkpoint/{}/{artifact_id}",
                    checkpoint.checkpoint_id
                ),
                checksum: format!("sha256:{checksum:x}"),
                size_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                created_at: chrono::Utc::now(),
                producer: "runtime-checkpoint".to_owned(),
                redaction_status: RedactionStatus::Raw,
                retention_policy: "restore_only_owner_access".to_owned(),
                provenance_refs: vec![checkpoint_event_id],
            };
            self.store.store_artifact(&artifact, bytes).await?;
            checkpoint.artifact_refs.push(artifact_id);
        }

        let payload_ref = checkpoint.artifact_refs.first().copied();
        let mut event = agent_event(
            self.next_sequence_no(),
            task,
            RuntimeEventType::CheckpointCreated,
            RuntimeEventSource::Runtime,
            json!({
                "summary": "workspace before-image persisted before tool side effect",
                "checkpoint": checkpoint,
            }),
        );
        event.id = checkpoint_event_id;
        event.turn_id = request.turn_id;
        event.payload_ref = payload_ref;
        self.record_event(event).await
    }

    async fn record_tool_report(
        &self,
        task: &HostedAgentTask,
        report: &golutra_tools::ToolExecutionReport,
    ) -> Result<(), ClientError> {
        let mut event = agent_event(
            self.next_sequence_no(),
            task,
            RuntimeEventType::ToolCompleted,
            RuntimeEventSource::Tool,
            json!({
                "summary": report.envelope.summary,
                "envelope": report.envelope,
                "changed_files": report.changed_files,
            }),
        );
        if let Some(turn_id) = report
            .artifacts
            .iter()
            .find_map(|artifact| artifact.turn_id)
        {
            event.turn_id = Some(turn_id);
        }
        let tool_event_id = event.id;
        for artifact in &report.artifacts {
            let content = report
                .artifact_contents
                .iter()
                .find(|content| content.artifact_id == artifact.artifact_id)
                .ok_or_else(|| {
                    ClientError::TaskExecution(format!(
                        "artifact {} has no durable content",
                        artifact.artifact_id
                    ))
                })?;
            let mut artifact = artifact.clone();
            if !artifact.provenance_refs.contains(&tool_event_id) {
                artifact.provenance_refs.push(tool_event_id);
            }
            self.store.store_artifact(&artifact, &content.bytes).await?;
        }
        for evidence in &report.evidence {
            let mut evidence = evidence.clone();
            if !evidence.source_event_refs.contains(&tool_event_id) {
                evidence.source_event_refs.push(tool_event_id);
            }
            self.store.store_evidence(&evidence).await?;
        }
        self.record_event(event).await
    }

    async fn finish_lane(
        &self,
        task: &HostedAgentTask,
        status: TaskStatus,
    ) -> Result<(), ClientError> {
        let mut lane_manager = self.lane_manager.lock().await;
        let transition = lane_manager.finish_task(task.session_id, status, self.next_sequence_no());
        drop(lane_manager);
        match transition {
            Ok(mut transition) => {
                transition.event.payload["summary"] =
                    json!(format!("runtime task finished with {status:?}"));
                transition.event.payload["status"] = json!(status);
                self.record_event(transition.event).await
            }
            Err(RuntimeLaneError::LaneNotFound) => {
                let event_type = if status == TaskStatus::Cancelled {
                    RuntimeEventType::TaskAborted
                } else {
                    RuntimeEventType::TaskCompleted
                };
                self.record_event(agent_event(
                    self.next_sequence_no(),
                    task,
                    event_type,
                    RuntimeEventSource::Runtime,
                    json!({
                        "summary": format!("persisted runtime task finished with {status:?}"),
                        "status": status,
                    }),
                ))
                .await
            }
            Err(error) => Err(error.into()),
        }
    }

    async fn record_task_execution_failure(
        &self,
        task: &HostedAgentTask,
        error: ClientError,
    ) -> Result<(), ClientError> {
        let active_turn_id = self
            .lane_manager
            .lock()
            .await
            .lane(task.session_id)
            .and_then(|lane| lane.active_turn_id)
            .unwrap_or(task.turn_id);
        let failure_task = HostedAgentTask {
            turn_id: active_turn_id,
            ..task.clone()
        };
        let objective = self.objective_for_task_turn(task, active_turn_id).await?;
        if matches!(error, ClientError::TaskCancelled) {
            self.evaluate_completed_task(
                &failure_task,
                HostedTaskEvaluation {
                    objective: &objective,
                    task_status: TaskStatus::Cancelled,
                    verification: None,
                    tool_reports: &[],
                    failure_summary: Some("task cancelled by controller".to_owned()),
                    latency: Duration::ZERO,
                },
            )
            .await?;
            return self.finish_lane(&failure_task, TaskStatus::Cancelled).await;
        }
        let error_summary = compact_event_summary(&error.to_string());
        self.record_event(agent_event(
            self.next_sequence_no(),
            &failure_task,
            RuntimeEventType::LoopDecided,
            RuntimeEventSource::Runtime,
            json!({
                "summary": format!("runtime task execution failed: {error_summary}"),
                "error": error.to_string(),
            }),
        ))
        .await?;
        self.evaluate_completed_task(
            &failure_task,
            HostedTaskEvaluation {
                objective: &objective,
                task_status: TaskStatus::Failed,
                verification: None,
                tool_reports: &[],
                failure_summary: Some(error.to_string()),
                latency: Duration::ZERO,
            },
        )
        .await?;
        self.finish_lane(&failure_task, TaskStatus::Failed).await
    }

    async fn objective_for_task_turn(
        &self,
        task: &HostedAgentTask,
        turn_id: TurnId,
    ) -> Result<String, ClientError> {
        if turn_id == task.turn_id {
            return Ok(prompt_from_payload(&task.payload));
        }
        let events = self
            .store
            .load_recent_events(
                task.session_id,
                Some(task.task_id),
                None,
                MAX_HISTORY_SOURCE_EVENTS,
            )
            .await?;
        Ok(events
            .iter()
            .rev()
            .find(|event| {
                event.turn_id == Some(turn_id) && event.event_type == RuntimeEventType::TurnStarted
            })
            .and_then(|event| event.payload.get("prompt"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| prompt_from_payload(&task.payload)))
    }

    fn execution_workspace_root(&self) -> Result<PathBuf, ClientError> {
        self.workspace_root.clone().map(Ok).unwrap_or_else(|| {
            std::env::current_dir().map_err(|error| ClientError::Io(error.to_string()))
        })
    }

    fn workspace_root_string(&self) -> Option<String> {
        self.workspace_root
            .as_ref()
            .map(|path| path.to_string_lossy().to_string())
    }

    fn ensure_thread_in_workspace(&self, thread: &ThreadRecord) -> Result<(), ClientError> {
        let Some(workspace_root) = self.workspace_root_string() else {
            return Ok(());
        };
        if thread.workspace_root.as_deref() == Some(workspace_root.as_str()) {
            return Ok(());
        }
        Err(ClientError::InvalidSession(format!(
            "thread `{}` does not belong to workspace `{workspace_root}`",
            thread.thread_id
        )))
    }
}

#[cfg(test)]
mod tests;
