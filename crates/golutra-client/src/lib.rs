use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use async_trait::async_trait;
use golutra_context::{ContextBuilder, ContextContributor};
use golutra_core::{
    BusyPolicy, EventId, LoopAction, SessionId, TaskId, TaskStatus, TurnId, WorkspaceId,
};
use golutra_llm::{MockProvider, ProviderRole};
use golutra_policy::WorkspacePolicy;
use golutra_protocol::{
    CommandAck, EventFilter, RuntimeEvent, RuntimeEventSource, RuntimeEventType, RuntimeQuery,
    RuntimeQueryKind, SessionCommand, SessionCommandKind,
};
use golutra_runtime::{
    AgentLoop, AgentLoopTraceEvent, AgentTaskRequest, RuntimeLaneError, RuntimeLaneManager,
    is_active_status,
};
use golutra_store::{RuntimeStore, StoreError};
use golutra_tools::BasicToolExecutor;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::{Mutex, broadcast};
use uuid::Uuid;

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
}

#[async_trait]
pub trait RuntimeClient {
    async fn send_command(&self, command: SessionCommand) -> Result<CommandAck, ClientError>;
    async fn query(&self, query: RuntimeQuery) -> Result<Value, ClientError>;
    async fn replay_events(&self, filter: EventFilter) -> Result<Vec<Value>, ClientError>;

    async fn subscribe(&self, filter: EventFilter) -> Result<Vec<Value>, ClientError> {
        self.replay_events(filter).await
    }
}

#[derive(Debug, Clone)]
pub struct InProcessTransport {
    host: Arc<RuntimeHost>,
}

impl InProcessTransport {
    #[must_use]
    pub fn new(host: Arc<RuntimeHost>) -> Self {
        Self { host }
    }

    pub async fn in_memory() -> Result<Self, ClientError> {
        Ok(Self::new(RuntimeHost::in_memory().await?))
    }

    pub async fn for_current_workspace() -> Result<Self, ClientError> {
        let workspace =
            std::env::current_dir().map_err(|error| ClientError::Io(error.to_string()))?;
        Self::for_workspace(workspace).await
    }

    pub async fn for_workspace(workspace_root: impl AsRef<Path>) -> Result<Self, ClientError> {
        Ok(Self::new(RuntimeHost::for_workspace(workspace_root).await?))
    }

    #[must_use]
    pub fn default_session_id(&self) -> SessionId {
        self.host.default_session_id()
    }

    #[must_use]
    pub fn workspace_root(&self) -> Option<&Path> {
        self.host.workspace_root()
    }

    #[must_use]
    pub fn subscribe_live(&self, filter: EventFilter) -> broadcast::Receiver<RuntimeEvent> {
        self.host.subscribe_live(filter)
    }
}

#[async_trait]
impl RuntimeClient for InProcessTransport {
    async fn send_command(&self, command: SessionCommand) -> Result<CommandAck, ClientError> {
        self.host.clone().handle_command(command).await
    }

    async fn query(&self, query: RuntimeQuery) -> Result<Value, ClientError> {
        self.host.query(query).await
    }

    async fn replay_events(&self, filter: EventFilter) -> Result<Vec<Value>, ClientError> {
        self.host.replay_events(filter).await
    }
}

#[derive(Debug)]
pub struct RuntimeHost {
    store: RuntimeStore,
    lane_manager: Mutex<RuntimeLaneManager>,
    event_bus: broadcast::Sender<RuntimeEvent>,
    next_sequence_no: AtomicU64,
    workspace_id: WorkspaceId,
    workspace_root: Option<PathBuf>,
    default_session_id: SessionId,
}

#[derive(Debug, Clone)]
struct HostedAgentTask {
    session_id: SessionId,
    task_id: TaskId,
    turn_id: TurnId,
    payload: Value,
}

impl RuntimeHost {
    pub async fn in_memory() -> Result<Arc<Self>, ClientError> {
        let store = RuntimeStore::in_memory().await?;
        Self::from_store(store, None, SessionId::new()).await
    }

    pub async fn for_workspace(workspace_root: impl AsRef<Path>) -> Result<Arc<Self>, ClientError> {
        let resolver = SessionResolver::new(workspace_root.as_ref())?;
        let store = RuntimeStore::connect(&resolver.sqlite_url()).await?;
        let default_session_id = resolver.resolve_default_session()?;
        Self::from_store(store, Some(resolver.workspace_root), default_session_id).await
    }

    async fn from_store(
        store: RuntimeStore,
        workspace_root: Option<PathBuf>,
        default_session_id: SessionId,
    ) -> Result<Arc<Self>, ClientError> {
        let (event_bus, _) = broadcast::channel(512);
        let next_sequence_no = store.max_sequence_no().await?.saturating_add(1);
        Ok(Arc::new(Self {
            store,
            lane_manager: Mutex::new(RuntimeLaneManager::new()),
            event_bus,
            next_sequence_no: AtomicU64::new(next_sequence_no),
            workspace_id: WorkspaceId::new(),
            workspace_root,
            default_session_id,
        }))
    }

    #[must_use]
    pub fn default_session_id(&self) -> SessionId {
        self.default_session_id
    }

    #[must_use]
    pub fn workspace_root(&self) -> Option<&Path> {
        self.workspace_root.as_deref()
    }

    #[must_use]
    pub fn subscribe_live(&self, _filter: EventFilter) -> broadcast::Receiver<RuntimeEvent> {
        self.event_bus.subscribe()
    }

    pub async fn handle_command(
        self: Arc<Self>,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let session_id = command.session_id.unwrap_or(self.default_session_id);
        let command_id = command.command_id;
        let result = match command.kind {
            SessionCommandKind::Create => {
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
                CommandAck {
                    command_id,
                    accepted: true,
                    reason: Some(format!("session {session_id} is ready")),
                }
            }
            SessionCommandKind::Prompt => self.handle_prompt(session_id, command).await?,
            SessionCommandKind::Abort => {
                self.handle_lane_command(session_id, command_id, "abort")
                    .await?
            }
            SessionCommandKind::Pause => {
                self.handle_lane_command(session_id, command_id, "pause")
                    .await?
            }
            SessionCommandKind::Resume => {
                self.handle_lane_command(session_id, command_id, "resume")
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
        Ok(result)
    }

    async fn handle_prompt(
        self: Arc<Self>,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let task_id = TaskId::new();
        let turn_id = TurnId::new();
        let payload = command.payload.clone();
        let lane_manager = self.lane_manager.lock().await;
        if lane_manager.lane(session_id).is_some() {
            let decision = lane_manager.decide_busy_policy(
                session_id,
                command.command_id,
                &command.actor,
                BusyPolicy::Append,
            )?;
            let accepted = decision.applied_policy != BusyPolicy::Reject;
            self.record_event(host_event(
                self.next_sequence_no(),
                session_id,
                decision.affected_turn_id.map(|_| task_id),
                if accepted {
                    RuntimeEventType::BusyPolicyDecided
                } else {
                    RuntimeEventType::CommandRejected
                },
                RuntimeEventSource::Runtime,
                json!({
                    "summary": decision.reason,
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
        drop(lane_manager);
        if let Some(active_task_id) = self.persisted_active_task(session_id).await? {
            self.record_event(host_event(
                self.next_sequence_no(),
                session_id,
                Some(active_task_id),
                RuntimeEventType::CommandRejected,
                RuntimeEventSource::Runtime,
                json!({
                    "summary": "session already has an active persisted task",
                    "command_id": command.command_id.to_string(),
                    "payload": command.payload,
                }),
            ))
            .await?;
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some("session already has an active persisted task".to_owned()),
            });
        }

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
        self.record_event(with_command_payload(
            transition.event,
            command.command_id,
            payload.clone(),
        ))
        .await?;
        self.clone().spawn_agent_task(HostedAgentTask {
            session_id,
            task_id,
            turn_id,
            payload,
        });

        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some(format!("started task {task_id} in session {session_id}")),
        })
    }

    async fn handle_lane_command(
        &self,
        session_id: SessionId,
        command_id: golutra_core::CommandId,
        action: &str,
    ) -> Result<CommandAck, ClientError> {
        let mut lane_manager = self.lane_manager.lock().await;
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
            }
            Err(RuntimeLaneError::LaneNotFound) if action == "abort" => {
                let active_task_id = self.persisted_active_task(session_id).await?;
                self.record_event(host_event(
                    self.next_sequence_no(),
                    session_id,
                    active_task_id,
                    RuntimeEventType::TaskAborted,
                    RuntimeEventSource::Runtime,
                    json!({
                        "summary": "persisted runtime task aborted",
                        "command_id": command_id.to_string(),
                    }),
                ))
                .await?;
            }
            Err(error) => return Err(error.into()),
        }
        Ok(CommandAck {
            command_id,
            accepted: true,
            reason: Some(format!("{action} accepted in session {session_id}")),
        })
    }

    async fn query(&self, query: RuntimeQuery) -> Result<Value, ClientError> {
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
        };
        Ok(value)
    }

    async fn replay_events(&self, filter: EventFilter) -> Result<Vec<Value>, ClientError> {
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

    async fn record_event(&self, event: RuntimeEvent) -> Result<(), ClientError> {
        self.store.append_event(&event).await?;
        let _ = self.event_bus.send(event);
        Ok(())
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

    fn next_sequence_no(&self) -> u64 {
        self.next_sequence_no.fetch_add(1, Ordering::SeqCst)
    }

    fn spawn_agent_task(self: Arc<Self>, task: HostedAgentTask) {
        tokio::spawn(async move {
            if let Err(error) = self.clone().run_agent_task(task.clone()).await {
                let _ = self.record_task_execution_failure(&task, error).await;
            }
        });
    }

    async fn run_agent_task(self: Arc<Self>, task: HostedAgentTask) -> Result<(), ClientError> {
        let objective = prompt_from_payload(&task.payload);
        let workspace_root = self.execution_workspace_root()?;
        let policy = WorkspacePolicy::new(workspace_root)
            .map_err(|error| ClientError::TaskExecution(error.to_string()))?;
        let tool_executor = BasicToolExecutor::new(policy);
        let tool_names = tool_executor
            .registry()
            .contracts()
            .into_iter()
            .map(|contract| contract.tool_name.clone())
            .collect::<Vec<_>>();
        let provider_plan = mock_provider_plan(&task.payload, &objective);
        let agent_loop = AgentLoop::new(
            provider_plan.provider,
            ContextBuilder::default(),
            tool_executor,
        );
        let mut trace_events = Vec::new();
        let outcome = agent_loop
            .run_with_trace(
                AgentTaskRequest {
                    session_id: task.session_id,
                    task_id: task.task_id,
                    turn_id: task.turn_id,
                    objective: objective.clone(),
                    completion_criteria: vec![
                        "runtime task produces durable evidence or terminal verification"
                            .to_owned(),
                    ],
                    touched_code: provider_plan.touched_code,
                    contributors: vec![
                        ContextContributor {
                            name: "system".to_owned(),
                            role: ProviderRole::System,
                            content: "You are Golutra, a workspace coding agent.".to_owned(),
                            token_budget_hint: 64,
                        },
                        ContextContributor {
                            name: "objective".to_owned(),
                            role: ProviderRole::User,
                            content: objective,
                            token_budget_hint: 512,
                        },
                    ],
                    tools: tool_names,
                },
                |event| trace_events.push(event),
            )
            .await
            .map_err(|error| ClientError::TaskExecution(error.to_string()))?;

        for trace_event in trace_events {
            self.record_trace_event(&task, trace_event).await?;
        }
        for report in &outcome.tool_reports {
            for artifact in &report.artifacts {
                self.store.store_artifact(artifact).await?;
            }
            for evidence in &report.evidence {
                self.store.store_evidence(evidence).await?;
            }
            self.record_event(agent_event(
                self.next_sequence_no(),
                &task,
                RuntimeEventType::ToolCompleted,
                RuntimeEventSource::Tool,
                json!({
                    "summary": report.envelope.summary,
                    "envelope": report.envelope,
                    "changed_files": report.changed_files,
                }),
            ))
            .await?;
        }
        self.record_event(agent_event(
            self.next_sequence_no(),
            &task,
            RuntimeEventType::VerificationCompleted,
            RuntimeEventSource::Verifier,
            json!({
                "summary": format!("verification result: {:?}", outcome.verification.result),
                "record": outcome.verification,
            }),
        ))
        .await?;
        let terminal_status = task_status_from_loop_action(outcome.loop_decision.action);
        self.record_event(agent_event(
            self.next_sequence_no(),
            &task,
            RuntimeEventType::LoopDecided,
            RuntimeEventSource::Runtime,
            json!({
                "summary": outcome.loop_decision.reason,
                "record": outcome.loop_decision,
            }),
        ))
        .await?;
        self.finish_lane(&task, terminal_status).await
    }

    async fn record_trace_event(
        &self,
        task: &HostedAgentTask,
        trace_event: AgentLoopTraceEvent,
    ) -> Result<(), ClientError> {
        if let Some((event_type, source, payload)) = trace_event_payload(trace_event) {
            self.record_event(agent_event(
                self.next_sequence_no(),
                task,
                event_type,
                source,
                payload,
            ))
            .await?;
        }
        Ok(())
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
                transition.event.payload = json!({
                    "summary": format!("runtime task finished with {status:?}"),
                    "status": status,
                });
                self.record_event(transition.event).await
            }
            Err(RuntimeLaneError::LaneNotFound) => {
                self.record_event(agent_event(
                    self.next_sequence_no(),
                    task,
                    RuntimeEventType::TaskCompleted,
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
        self.record_event(agent_event(
            self.next_sequence_no(),
            task,
            RuntimeEventType::LoopDecided,
            RuntimeEventSource::Runtime,
            json!({
                "summary": "runtime task execution failed",
                "error": error.to_string(),
            }),
        ))
        .await?;
        self.finish_lane(task, TaskStatus::Failed).await
    }

    fn execution_workspace_root(&self) -> Result<PathBuf, ClientError> {
        self.workspace_root.clone().map(Ok).unwrap_or_else(|| {
            std::env::current_dir().map_err(|error| ClientError::Io(error.to_string()))
        })
    }
}

#[derive(Debug)]
struct SessionResolver {
    workspace_root: PathBuf,
    runtime_db: PathBuf,
    default_session_file: PathBuf,
}

impl SessionResolver {
    fn new(workspace_root: &Path) -> Result<Self, ClientError> {
        let workspace_root = workspace_root
            .canonicalize()
            .map_err(|error| ClientError::Io(error.to_string()))?;
        let golutra_dir = workspace_root.join(".golutra");
        fs::create_dir_all(&golutra_dir).map_err(|error| ClientError::Io(error.to_string()))?;
        Ok(Self {
            runtime_db: golutra_dir.join("runtime.sqlite"),
            default_session_file: golutra_dir.join("default-session"),
            workspace_root,
        })
    }

    fn sqlite_url(&self) -> String {
        format!("sqlite://{}", self.runtime_db.display())
    }

    fn resolve_default_session(&self) -> Result<SessionId, ClientError> {
        if self.default_session_file.exists() {
            let value = fs::read_to_string(&self.default_session_file)
                .map_err(|error| ClientError::Io(error.to_string()))?;
            let uuid = Uuid::parse_str(value.trim())
                .map_err(|error| ClientError::InvalidSession(error.to_string()))?;
            return Ok(SessionId(uuid));
        }

        let session_id = SessionId::new();
        fs::write(&self.default_session_file, session_id.to_string())
            .map_err(|error| ClientError::Io(error.to_string()))?;
        Ok(session_id)
    }
}

#[must_use]
pub fn projection_status(value: &Value) -> Option<TaskStatus> {
    value
        .get("task_status")
        .or_else(|| value.get("status"))
        .and_then(|status| serde_json::from_value(status.clone()).ok())
}

#[must_use]
pub fn default_session_id() -> SessionId {
    SessionId(Uuid::from_u128(1))
}

#[must_use]
pub fn event_sequence_no(value: &Value) -> Option<u64> {
    value.get("sequence_no").and_then(Value::as_u64)
}

#[derive(Debug, Clone)]
struct MockProviderPlan {
    provider: MockProvider,
    touched_code: bool,
}

fn mock_provider_plan(payload: &Value, objective: &str) -> MockProviderPlan {
    let lower = objective.to_ascii_lowercase();
    if lower.contains("write") || lower.contains("create") || payload.get("content").is_some() {
        return MockProviderPlan {
            provider: MockProvider::tool_call(
                "write_file",
                json!({
                    "path": string_payload(payload, "path", "golutra-agent-output.txt"),
                    "content": string_payload(payload, "content", "done\n"),
                }),
            ),
            touched_code: true,
        };
    }

    if lower.contains("read") {
        return MockProviderPlan {
            provider: MockProvider::tool_call(
                "read_file",
                json!({"path": string_payload(payload, "path", "README.md")}),
            ),
            touched_code: false,
        };
    }

    if lower.contains("sleep") {
        return MockProviderPlan {
            provider: MockProvider::tool_call("shell", json!({"command": "sleep 5"})),
            touched_code: false,
        };
    }

    if lower.contains("list") || lower.contains("ls") {
        return MockProviderPlan {
            provider: MockProvider::tool_call(
                "list_dir",
                json!({"path": string_payload(payload, "path", ".")}),
            ),
            touched_code: false,
        };
    }

    MockProviderPlan {
        provider: MockProvider::text_response("mock provider completed without tool calls"),
        touched_code: false,
    }
}

fn prompt_from_payload(payload: &Value) -> String {
    payload
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn string_payload(payload: &Value, key: &str, fallback: &str) -> String {
    payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(fallback)
        .to_owned()
}

fn task_status_from_loop_action(action: LoopAction) -> TaskStatus {
    match action {
        LoopAction::StopSuccess => TaskStatus::Completed,
        LoopAction::StopPartial => TaskStatus::Partial,
        LoopAction::StopFailed => TaskStatus::Failed,
        LoopAction::Blocked => TaskStatus::Blocked,
        LoopAction::Continue
        | LoopAction::Compact
        | LoopAction::Retry
        | LoopAction::Fallback
        | LoopAction::AskUser
        | LoopAction::Verify => TaskStatus::Partial,
    }
}

fn trace_event_payload(
    trace_event: AgentLoopTraceEvent,
) -> Option<(RuntimeEventType, RuntimeEventSource, Value)> {
    match trace_event {
        AgentLoopTraceEvent::ContextBuilt {
            contributors,
            planned_input_tokens,
        } => Some((
            RuntimeEventType::ContextBuilt,
            RuntimeEventSource::Runtime,
            json!({
                "summary": "context built for provider request",
                "contributors": contributors,
                "planned_input_tokens": planned_input_tokens,
            }),
        )),
        AgentLoopTraceEvent::ProviderStarted {
            provider_id,
            model_id,
        } => Some((
            RuntimeEventType::ProviderStarted,
            RuntimeEventSource::Provider,
            json!({
                "summary": "provider request started",
                "provider_id": provider_id,
                "model_id": model_id,
            }),
        )),
        AgentLoopTraceEvent::ProviderCompleted {
            provider_id,
            model_id,
            finish_reason,
            tool_call_count,
        } => Some((
            RuntimeEventType::ProviderCompleted,
            RuntimeEventSource::Provider,
            json!({
                "summary": "provider request completed",
                "provider_id": provider_id,
                "model_id": model_id,
                "finish_reason": finish_reason,
                "tool_call_count": tool_call_count,
            }),
        )),
        AgentLoopTraceEvent::ToolStarted { tool_name } => Some((
            RuntimeEventType::ToolStarted,
            RuntimeEventSource::Tool,
            json!({
                "summary": format!("tool {tool_name} started"),
                "tool_name": tool_name,
            }),
        )),
        AgentLoopTraceEvent::ToolCompleted { .. } => None,
    }
}

fn host_event(
    sequence_no: u64,
    session_id: SessionId,
    task_id: Option<TaskId>,
    event_type: RuntimeEventType,
    source: RuntimeEventSource,
    payload: Value,
) -> RuntimeEvent {
    RuntimeEvent {
        id: EventId::new(),
        sequence_no,
        session_id,
        turn_id: Some(TurnId::new()),
        task_id,
        parent_event_id: None,
        event_type,
        timestamp: chrono::Utc::now(),
        source,
        payload,
        payload_ref: None,
        durable: true,
    }
}

fn agent_event(
    sequence_no: u64,
    task: &HostedAgentTask,
    event_type: RuntimeEventType,
    source: RuntimeEventSource,
    payload: Value,
) -> RuntimeEvent {
    RuntimeEvent {
        id: EventId::new(),
        sequence_no,
        session_id: task.session_id,
        turn_id: Some(task.turn_id),
        task_id: Some(task.task_id),
        parent_event_id: None,
        event_type,
        timestamp: chrono::Utc::now(),
        source,
        payload,
        payload_ref: None,
        durable: true,
    }
}

fn with_command_payload(
    mut event: RuntimeEvent,
    command_id: golutra_core::CommandId,
    payload: Value,
) -> RuntimeEvent {
    event.payload = json!({
        "summary": event
            .payload
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or("runtime host accepted command"),
        "command_id": command_id.to_string(),
        "payload": payload,
        "runtime": event.payload,
    });
    event
}

#[cfg(test)]
mod tests {
    use std::fs;

    use golutra_core::{Actor, ActorKind, CommandId, QueryId};
    use golutra_protocol::RuntimeQueryKind;
    use tempfile::tempdir;
    use tokio::time::{Duration, sleep};

    use super::*;

    #[tokio::test]
    async fn command_query_and_subscribe_share_state() {
        let transport = InProcessTransport::in_memory().await.expect("transport");
        let session_id = SessionId::new();
        let command = command(session_id, "list workspace");

        let ack = transport.send_command(command).await.expect("accepted");
        let state = wait_for_status(&transport, session_id, TaskStatus::Completed).await;
        let events = transport
            .replay_events(EventFilter {
                session_id,
                task_id: None,
                after_sequence_no: None,
            })
            .await
            .expect("events");

        assert!(ack.accepted);
        assert_eq!(projection_status(&state), Some(TaskStatus::Completed));
        assert!(events.len() >= 7);
    }

    #[tokio::test]
    async fn workspace_transport_reuses_default_session_and_sqlite_events() {
        let workspace = tempdir().expect("workspace");
        let first = InProcessTransport::for_workspace(workspace.path())
            .await
            .expect("first transport");
        let session_id = first.default_session_id();
        first
            .send_command(command(session_id, "list workspace"))
            .await
            .expect("command");
        wait_for_status(&first, session_id, TaskStatus::Completed).await;

        let second = InProcessTransport::for_workspace(workspace.path())
            .await
            .expect("second transport");
        let events = second
            .replay_events(EventFilter {
                session_id: second.default_session_id(),
                task_id: None,
                after_sequence_no: None,
            })
            .await
            .expect("events");

        assert_eq!(second.default_session_id(), session_id);
        assert!(events.len() >= 7);
        assert!(workspace.path().join(".golutra/runtime.sqlite").exists());
    }

    #[tokio::test]
    async fn prompt_runs_mock_agent_loop_and_writes_file() {
        let workspace = tempdir().expect("workspace");
        let transport = InProcessTransport::for_workspace(workspace.path())
            .await
            .expect("transport");
        let session_id = transport.default_session_id();

        let ack = transport
            .send_command(command_with_payload(
                session_id,
                json!({
                    "prompt": "write file",
                    "path": "result.txt",
                    "content": "done",
                }),
            ))
            .await
            .expect("command");
        let state = wait_for_status(&transport, session_id, TaskStatus::Completed).await;
        let debug = transport
            .query(RuntimeQuery {
                query_id: QueryId::new(),
                session_id,
                task_id: None,
                kind: RuntimeQueryKind::DebugProjection,
                requester: ActorKind::Cli,
                cursor: None,
                timestamp: chrono::Utc::now(),
            })
            .await
            .expect("debug projection");

        assert!(ack.accepted);
        assert_eq!(projection_status(&state), Some(TaskStatus::Completed));
        assert_eq!(
            fs::read_to_string(workspace.path().join("result.txt")).expect("file"),
            "done"
        );
        assert!(
            debug["tool_results"]
                .as_array()
                .is_some_and(|items| !items.is_empty())
        );
        assert!(
            debug["artifacts"]
                .as_array()
                .is_some_and(|items| !items.is_empty())
        );
    }

    #[tokio::test]
    async fn persisted_active_task_rejects_new_prompt_and_accepts_abort() {
        let workspace = tempdir().expect("workspace");
        let host = RuntimeHost::for_workspace(workspace.path())
            .await
            .expect("host");
        let session_id = host.default_session_id();
        host.record_event(host_event(
            host.next_sequence_no(),
            session_id,
            Some(TaskId::new()),
            RuntimeEventType::TaskCreated,
            RuntimeEventSource::Runtime,
            json!({"summary": "persisted active task"}),
        ))
        .await
        .expect("event");

        let second = InProcessTransport::for_workspace(workspace.path())
            .await
            .expect("second transport");
        let rejected = second
            .send_command(command(second.default_session_id(), "second"))
            .await
            .expect("rejected command ack");
        let abort = second
            .send_command(SessionCommand {
                command_id: CommandId::new(),
                session_id: Some(second.default_session_id()),
                kind: SessionCommandKind::Abort,
                idempotency_key: "abort".to_owned(),
                actor: Actor {
                    kind: ActorKind::Cli,
                    id: "test".to_owned(),
                },
                payload: json!({}),
                timestamp: chrono::Utc::now(),
            })
            .await
            .expect("abort");
        let state = second
            .query(RuntimeQuery {
                query_id: QueryId::new(),
                session_id,
                task_id: None,
                kind: RuntimeQueryKind::SessionState,
                requester: ActorKind::Cli,
                cursor: None,
                timestamp: chrono::Utc::now(),
            })
            .await
            .expect("state");

        assert!(!rejected.accepted);
        assert!(abort.accepted);
        assert_eq!(projection_status(&state), Some(TaskStatus::Aborting));
    }

    fn command(session_id: SessionId, prompt: &str) -> SessionCommand {
        command_with_payload(session_id, json!({"prompt": prompt}))
    }

    fn command_with_payload(session_id: SessionId, payload: Value) -> SessionCommand {
        SessionCommand {
            command_id: CommandId::new(),
            session_id: Some(session_id),
            kind: SessionCommandKind::Prompt,
            idempotency_key: "test".to_owned(),
            actor: Actor {
                kind: ActorKind::Cli,
                id: "test".to_owned(),
            },
            payload,
            timestamp: chrono::Utc::now(),
        }
    }

    async fn wait_for_status(
        transport: &InProcessTransport,
        session_id: SessionId,
        expected: TaskStatus,
    ) -> Value {
        for _ in 0..40 {
            let state = transport
                .query(RuntimeQuery {
                    query_id: QueryId::new(),
                    session_id,
                    task_id: None,
                    kind: RuntimeQueryKind::SessionState,
                    requester: ActorKind::Cli,
                    cursor: None,
                    timestamp: chrono::Utc::now(),
                })
                .await
                .expect("state");
            if projection_status(&state) == Some(expected) {
                return state;
            }
            sleep(Duration::from_millis(50)).await;
        }
        panic!("timed out waiting for status {expected:?}");
    }
}
