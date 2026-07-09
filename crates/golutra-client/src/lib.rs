use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use async_trait::async_trait;
use golutra_config::load_provider_runtime_env;
use golutra_context::{ContextBuilder, ContextContributor};
use golutra_core::{
    BusyPolicy, EventId, LoopAction, SessionId, TaskId, TaskStatus, ThreadId, TurnId, WorkspaceId,
};
use golutra_llm::{ConfiguredProvider, MockProvider, ProviderError, ProviderRole};
use golutra_policy::WorkspacePolicy;
use golutra_protocol::{
    CommandAck, EventFilter, RuntimeEvent, RuntimeEventSource, RuntimeEventType, RuntimeQuery,
    RuntimeQueryKind, SessionCommand, SessionCommandKind,
};
use golutra_runtime::{
    AgentLoop, AgentLoopTraceEvent, AgentTaskRequest, RuntimeLaneError, RuntimeLaneManager,
    WorkspaceCheckpointManager, is_active_status,
};
use golutra_store::{RuntimeStore, StoreError, ThreadRecord};
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
    pub fn default_thread_id(&self) -> ThreadId {
        self.host.default_thread_id()
    }

    #[must_use]
    pub fn workspace_root(&self) -> Option<&Path> {
        self.host.workspace_root()
    }

    #[must_use]
    pub fn subscribe_live(&self, filter: EventFilter) -> broadcast::Receiver<RuntimeEvent> {
        self.host.subscribe_live(filter)
    }

    pub async fn list_threads(&self, limit: u32) -> Result<Vec<ThreadRecord>, ClientError> {
        self.host.list_threads(limit).await
    }

    pub async fn resume_thread(&self, thread_id: ThreadId) -> Result<ThreadRecord, ClientError> {
        self.host.resume_thread(thread_id).await
    }

    pub async fn fork_thread(&self, thread_id: ThreadId) -> Result<ThreadRecord, ClientError> {
        self.host.fork_thread(thread_id).await
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
    default_thread_id: ThreadId,
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
        let default_session_id = SessionId::new();
        let default_thread_id = ThreadId::new();
        ensure_thread_record(&store, None, default_thread_id, default_session_id).await?;
        Self::from_store(store, None, default_session_id, default_thread_id).await
    }

    pub async fn for_workspace(workspace_root: impl AsRef<Path>) -> Result<Arc<Self>, ClientError> {
        let resolver = SessionResolver::new(workspace_root.as_ref())?;
        let store = RuntimeStore::connect(&resolver.sqlite_url()).await?;
        let default_session_id = resolver.resolve_default_session()?;
        let default_thread_id = resolver.resolve_default_thread()?;
        let default_thread = resolver
            .repair_default_thread(&store, default_thread_id, default_session_id)
            .await?;
        Self::from_store(
            store,
            Some(resolver.workspace_root),
            default_thread.session_id,
            default_thread.thread_id,
        )
        .await
    }

    async fn from_store(
        store: RuntimeStore,
        workspace_root: Option<PathBuf>,
        default_session_id: SessionId,
        default_thread_id: ThreadId,
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
            default_thread_id,
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
        self.upsert_current_thread(session_id, &payload).await?;
        let lane_manager = self.lane_manager.lock().await;
        if lane_manager
            .lane(session_id)
            .is_some_and(|lane| is_active_status(lane.status))
        {
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

    pub async fn list_threads(&self, limit: u32) -> Result<Vec<ThreadRecord>, ClientError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let workspace_root = self.workspace_root_string();
        let fetch_limit = limit.saturating_add(20);
        let threads = self
            .store
            .list_threads(workspace_root.as_deref(), fetch_limit)
            .await?
            .into_iter()
            .filter(|thread| !is_placeholder_thread(thread))
            .take(limit as usize)
            .collect();
        Ok(threads)
    }

    pub async fn resume_thread(&self, thread_id: ThreadId) -> Result<ThreadRecord, ClientError> {
        let thread = self.store.thread_by_id(thread_id).await?.ok_or_else(|| {
            ClientError::InvalidSession(format!("thread `{thread_id}` not found"))
        })?;
        self.ensure_thread_in_workspace(&thread)?;
        self.write_default_thread_files(thread.thread_id, thread.session_id)?;
        Ok(thread)
    }

    pub async fn fork_thread(&self, thread_id: ThreadId) -> Result<ThreadRecord, ClientError> {
        let parent = self.store.thread_by_id(thread_id).await?.ok_or_else(|| {
            ClientError::InvalidSession(format!("thread `{thread_id}` not found"))
        })?;
        self.ensure_thread_in_workspace(&parent)?;
        let now = chrono::Utc::now();
        let child = ThreadRecord {
            thread_id: ThreadId::new(),
            session_id: SessionId::new(),
            parent_thread_id: Some(parent.thread_id),
            workspace_root: parent.workspace_root.clone(),
            title: format!("Fork of {}", parent.title),
            preview: parent.preview.clone(),
            created_at: now,
            updated_at: now,
            recency_at: now,
            archived: false,
        };
        self.store.upsert_thread(&child).await?;
        self.write_default_thread_files(child.thread_id, child.session_id)?;
        Ok(child)
    }

    async fn upsert_current_thread(
        &self,
        session_id: SessionId,
        payload: &Value,
    ) -> Result<(), ClientError> {
        let now = chrono::Utc::now();
        let existing = self.store.thread_by_session(session_id).await?;
        let payload_thread_id = thread_id_from_payload(payload);
        let default_thread = if existing.is_none() && payload_thread_id.is_none() {
            self.store.thread_by_id(self.default_thread_id).await?
        } else {
            None
        };
        let source_thread = existing.as_ref().or(default_thread.as_ref());
        let thread = ThreadRecord {
            thread_id: existing
                .as_ref()
                .map(|thread| thread.thread_id)
                .or(payload_thread_id)
                .or(default_thread.as_ref().map(|thread| thread.thread_id))
                .unwrap_or(self.default_thread_id),
            session_id,
            parent_thread_id: existing
                .as_ref()
                .or(default_thread.as_ref())
                .and_then(|thread| thread.parent_thread_id),
            workspace_root: self.workspace_root_string(),
            title: thread_title_for_prompt(source_thread, payload),
            preview: preview_from_payload(payload),
            created_at: existing
                .as_ref()
                .or(default_thread.as_ref())
                .map(|thread| thread.created_at)
                .unwrap_or(now),
            updated_at: now,
            recency_at: now,
            archived: false,
        };
        self.store.upsert_thread(&thread).await?;
        Ok(())
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

    async fn context_contributors_for_task(
        &self,
        session_id: SessionId,
        current_task_id: TaskId,
        objective: String,
    ) -> Result<Vec<ContextContributor>, ClientError> {
        let mut contributors = vec![ContextContributor {
            name: "system".to_owned(),
            role: ProviderRole::System,
            content: system_prompt(),
            token_budget_hint: 64,
        }];

        if let Some(history) = self
            .conversation_history_summary(session_id, current_task_id)
            .await?
        {
            contributors.push(ContextContributor {
                name: "conversation_history".to_owned(),
                role: ProviderRole::System,
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
        let events = self.store.load_events(session_id, None, None).await?;
        let lines = events
            .iter()
            .filter(|event| event.task_id != Some(current_task_id))
            .filter_map(conversation_history_line)
            .collect::<Vec<_>>();

        if lines.is_empty() {
            return Ok(None);
        }

        Ok(Some(format!(
            "Previous conversation in this workspace session:\n{}",
            compact_history_lines(lines)
        )))
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
            mock_provider_plan(self.workspace_root.as_deref(), &task.payload, &objective)
                .map_err(|error| ClientError::TaskExecution(error.to_string()))?;
        let agent_loop = AgentLoop::new(
            provider_plan.provider,
            ContextBuilder::default(),
            tool_executor,
        );
        let contributors = self
            .context_contributors_for_task(task.session_id, task.task_id, objective.clone())
            .await?;
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
                    contributors,
                    tools: if provider_plan.workspace_tools_enabled {
                        workspace_tool_names
                    } else {
                        Vec::new()
                    },
                },
                |event| trace_events.push(event),
            )
            .await
            .map_err(|error| ClientError::TaskExecution(error.to_string()))?;

        for trace_event in trace_events {
            self.record_trace_event(&task, trace_event).await?;
        }
        let mut changed_files = Vec::new();
        let mut last_tool_event_id = EventId::new();
        for report in &outcome.tool_reports {
            changed_files.extend(report.changed_files.clone());
            for artifact in &report.artifacts {
                self.store.store_artifact(artifact).await?;
            }
            for evidence in &report.evidence {
                self.store.store_evidence(evidence).await?;
            }
            let event = agent_event(
                self.next_sequence_no(),
                &task,
                RuntimeEventType::ToolCompleted,
                RuntimeEventSource::Tool,
                json!({
                    "summary": report.envelope.summary,
                    "envelope": report.envelope,
                    "changed_files": report.changed_files,
                }),
            );
            last_tool_event_id = event.id;
            self.record_event(event).await?;
        }
        if !changed_files.is_empty() {
            let checkpoint = WorkspaceCheckpointManager::new(
                workspace_root.clone(),
                workspace_root.join(".golutra/checkpoints"),
            )
            .create_checkpoint(
                self.workspace_id,
                task.task_id,
                task.turn_id,
                &changed_files,
                last_tool_event_id,
            )
            .map_err(|error| ClientError::TaskExecution(error.to_string()))?;
            self.record_event(agent_event(
                self.next_sequence_no(),
                &task,
                RuntimeEventType::CheckpointCreated,
                RuntimeEventSource::Runtime,
                json!({
                    "summary": "workspace checkpoint created",
                    "checkpoint": checkpoint,
                }),
            ))
            .await?;
        }
        if let Some(final_message) = outcome
            .final_message
            .as_ref()
            .filter(|message| !message.trim().is_empty())
        {
            self.record_event(agent_event(
                self.next_sequence_no(),
                &task,
                RuntimeEventType::AssistantMessage,
                RuntimeEventSource::Runtime,
                json!({
                    "summary": compact_event_summary(final_message),
                    "content": final_message,
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
        let error_summary = compact_event_summary(&error.to_string());
        self.record_event(agent_event(
            self.next_sequence_no(),
            task,
            RuntimeEventType::LoopDecided,
            RuntimeEventSource::Runtime,
            json!({
                "summary": format!("runtime task execution failed: {error_summary}"),
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

    fn write_default_thread_files(
        &self,
        thread_id: ThreadId,
        session_id: SessionId,
    ) -> Result<(), ClientError> {
        let Some(workspace_root) = &self.workspace_root else {
            return Ok(());
        };
        let golutra_dir = workspace_root.join(".golutra");
        fs::create_dir_all(&golutra_dir).map_err(|error| ClientError::Io(error.to_string()))?;
        fs::write(golutra_dir.join("default-thread"), thread_id.to_string())
            .map_err(|error| ClientError::Io(error.to_string()))?;
        fs::write(golutra_dir.join("default-session"), session_id.to_string())
            .map_err(|error| ClientError::Io(error.to_string()))?;
        Ok(())
    }
}

#[derive(Debug)]
struct SessionResolver {
    workspace_root: PathBuf,
    runtime_db: PathBuf,
    default_session_file: PathBuf,
    default_thread_file: PathBuf,
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
            default_thread_file: golutra_dir.join("default-thread"),
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

    fn resolve_default_thread(&self) -> Result<ThreadId, ClientError> {
        if self.default_thread_file.exists() {
            let value = fs::read_to_string(&self.default_thread_file)
                .map_err(|error| ClientError::Io(error.to_string()))?;
            return value
                .trim()
                .parse()
                .map_err(|error: uuid::Error| ClientError::InvalidSession(error.to_string()));
        }

        let thread_id = ThreadId::new();
        fs::write(&self.default_thread_file, thread_id.to_string())
            .map_err(|error| ClientError::Io(error.to_string()))?;
        Ok(thread_id)
    }

    async fn repair_default_thread(
        &self,
        store: &RuntimeStore,
        default_thread_id: ThreadId,
        default_session_id: SessionId,
    ) -> Result<ThreadRecord, ClientError> {
        let workspace_root = self.workspace_root.to_string_lossy().to_string();
        let default_thread_exists =
            if let Some(thread) = store.thread_by_id(default_thread_id).await? {
                if thread.workspace_root.as_deref() == Some(workspace_root.as_str()) {
                    self.write_default_ids(thread.thread_id, thread.session_id)?;
                    return Ok(thread);
                }
                true
            } else {
                false
            };

        if let Some(thread) = store
            .list_threads(Some(&workspace_root), 1)
            .await?
            .into_iter()
            .next()
        {
            self.write_default_ids(thread.thread_id, thread.session_id)?;
            return Ok(thread);
        }

        let bootstrap_thread_id = if default_thread_exists {
            ThreadId::new()
        } else {
            default_thread_id
        };
        let thread = ensure_thread_record(
            store,
            Some(workspace_root),
            bootstrap_thread_id,
            default_session_id,
        )
        .await?;
        self.write_default_ids(thread.thread_id, thread.session_id)?;
        Ok(thread)
    }

    fn write_default_ids(
        &self,
        thread_id: ThreadId,
        session_id: SessionId,
    ) -> Result<(), ClientError> {
        fs::write(&self.default_thread_file, thread_id.to_string())
            .map_err(|error| ClientError::Io(error.to_string()))?;
        fs::write(&self.default_session_file, session_id.to_string())
            .map_err(|error| ClientError::Io(error.to_string()))?;
        Ok(())
    }
}

async fn ensure_thread_record(
    store: &RuntimeStore,
    workspace_root: Option<String>,
    thread_id: ThreadId,
    session_id: SessionId,
) -> Result<ThreadRecord, ClientError> {
    if let Some(thread) = store.thread_by_id(thread_id).await? {
        return Ok(thread);
    }
    let now = chrono::Utc::now();
    let thread = ThreadRecord {
        thread_id,
        session_id,
        parent_thread_id: None,
        workspace_root,
        title: "New thread".to_owned(),
        preview: "Ready to start a task".to_owned(),
        created_at: now,
        updated_at: now,
        recency_at: now,
        archived: false,
    };
    store.upsert_thread(&thread).await?;
    Ok(thread)
}

fn thread_id_from_payload(payload: &Value) -> Option<ThreadId> {
    payload
        .get("_thread_id")
        .and_then(Value::as_str)
        .and_then(|value| value.parse().ok())
}

fn is_placeholder_thread(thread: &ThreadRecord) -> bool {
    thread.parent_thread_id.is_none()
        && thread.title == "New thread"
        && thread.preview == "Ready to start a task"
}

fn thread_title_for_prompt(source_thread: Option<&ThreadRecord>, payload: &Value) -> String {
    let current_title = source_thread
        .map(|thread| thread.title.trim())
        .unwrap_or_default();
    let should_refresh_title = current_title.is_empty()
        || source_thread.is_some_and(is_placeholder_thread)
        || current_title == "Untitled thread"
        || current_title == "Fork of New thread";

    if should_refresh_title {
        title_from_payload(payload)
    } else {
        current_title.to_owned()
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
    provider: ConfiguredProvider,
    touched_code: bool,
    workspace_tools_enabled: bool,
}

fn mock_provider_plan(
    workspace_root: Option<&Path>,
    payload: &Value,
    objective: &str,
) -> Result<MockProviderPlan, ProviderError> {
    let provider_env = workspace_root.and_then(|root| load_provider_runtime_env(root).ok());
    let lower = objective.to_ascii_lowercase();
    if lower.contains("write") || lower.contains("create") || payload.get("content").is_some() {
        let write_args = mock_write_file_args(payload, objective);
        return Ok(MockProviderPlan {
            provider: resolve_configured_provider(
                provider_env.as_ref(),
                MockProvider::tool_call(
                    "write_file",
                    json!({
                        "path": write_args.path,
                        "content": write_args.content,
                    }),
                ),
            )?,
            touched_code: true,
            workspace_tools_enabled: true,
        });
    }

    if lower.contains("read") {
        return Ok(MockProviderPlan {
            provider: resolve_configured_provider(
                provider_env.as_ref(),
                MockProvider::tool_call(
                    "read_file",
                    json!({"path": string_payload(payload, "path", "README.md")}),
                ),
            )?,
            touched_code: false,
            workspace_tools_enabled: true,
        });
    }

    if lower.contains("sleep") {
        return Ok(MockProviderPlan {
            provider: resolve_configured_provider(
                provider_env.as_ref(),
                MockProvider::tool_call("shell", json!({"command": "sleep 5"})),
            )?,
            touched_code: false,
            workspace_tools_enabled: true,
        });
    }

    if lower.contains("list") || lower.contains("ls") {
        return Ok(MockProviderPlan {
            provider: resolve_configured_provider(
                provider_env.as_ref(),
                MockProvider::tool_call(
                    "list_dir",
                    json!({"path": string_payload(payload, "path", ".")}),
                ),
            )?,
            touched_code: false,
            workspace_tools_enabled: true,
        });
    }

    Ok(MockProviderPlan {
        provider: resolve_configured_provider(
            provider_env.as_ref(),
            MockProvider::text_response("mock provider completed without tool calls"),
        )?,
        touched_code: false,
        workspace_tools_enabled: prompt_requests_workspace_tools(payload, objective),
    })
}

fn prompt_requests_workspace_tools(payload: &Value, objective: &str) -> bool {
    if payload.get("path").is_some()
        || payload.get("content").is_some()
        || payload.get("command").is_some()
    {
        return true;
    }

    let lower = objective.to_ascii_lowercase();
    const ENGLISH_MARKERS: &[&str] = &[
        "write",
        "create",
        "edit",
        "modify",
        "update",
        "delete",
        "read",
        "list",
        "search",
        "find",
        "inspect",
        "run",
        "test",
        "build",
        "fix",
        "debug",
        "refactor",
        "file",
        "code",
        "workspace",
        "diff",
        "commit",
        "shell",
    ];
    const CJK_MARKERS: &[&str] = &[
        "写",
        "创建",
        "修改",
        "更新",
        "删除",
        "读取",
        "读",
        "列出",
        "搜索",
        "查找",
        "检查",
        "运行",
        "测试",
        "构建",
        "修复",
        "重构",
        "文件",
        "代码",
        "工作区",
        "提交",
    ];

    ENGLISH_MARKERS.iter().any(|marker| lower.contains(marker))
        || CJK_MARKERS.iter().any(|marker| objective.contains(marker))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MockWriteFileArgs {
    path: String,
    content: String,
}

fn mock_write_file_args(payload: &Value, objective: &str) -> MockWriteFileArgs {
    let parsed = parse_mock_write_file_prompt(objective);
    MockWriteFileArgs {
        path: non_empty_string_payload(payload, "path")
            .or_else(|| parsed.as_ref().map(|parsed| parsed.path.clone()))
            .unwrap_or_else(|| "golutra-agent-output.txt".to_owned()),
        content: non_empty_string_payload(payload, "content")
            .or_else(|| parsed.map(|parsed| parsed.content))
            .unwrap_or_else(|| "done\n".to_owned()),
    }
}

fn parse_mock_write_file_prompt(objective: &str) -> Option<MockWriteFileArgs> {
    let objective = objective.trim();
    let lower = objective.to_ascii_lowercase();
    let marker = " with content ";
    let marker_index = lower.find(marker)?;
    let (path_part, content_part_with_marker) = objective.split_at(marker_index);
    let content = clean_mock_prompt_segment(&content_part_with_marker[marker.len()..]);
    let path = parse_mock_write_path(path_part)?;
    if content.is_empty() {
        return None;
    }
    Some(MockWriteFileArgs { path, content })
}

fn parse_mock_write_path(path_part: &str) -> Option<String> {
    let tokens = path_part.split_whitespace().collect::<Vec<_>>();
    let command_index = tokens
        .iter()
        .position(|token| matches!(token.to_ascii_lowercase().as_str(), "write" | "create"))?;
    let candidate = match tokens
        .get(command_index + 1)
        .map(|token| token.to_ascii_lowercase())
    {
        Some(value) if value == "file" => tokens.get(command_index + 2),
        Some(_) => tokens.get(command_index + 1),
        None => None,
    }?;
    let path = clean_mock_prompt_segment(candidate);
    if path.is_empty() { None } else { Some(path) }
}

fn clean_mock_prompt_segment(value: &str) -> String {
    value
        .trim()
        .trim_matches(|character| matches!(character, '"' | '\'' | '`' | ',' | ';' | ':'))
        .to_owned()
}

fn conversation_history_line(event: &RuntimeEvent) -> Option<String> {
    match event.event_type {
        RuntimeEventType::TaskCreated => event
            .payload
            .get("payload")
            .and_then(|payload| payload.get("prompt"))
            .and_then(Value::as_str)
            .filter(|prompt| !prompt.trim().is_empty())
            .map(|prompt| format!("User: {}", compact_history_text(prompt, 240))),
        RuntimeEventType::AssistantMessage => event
            .payload
            .get("content")
            .and_then(Value::as_str)
            .filter(|message| !message.trim().is_empty())
            .map(|message| format!("Golutra: {}", compact_history_text(message, 360))),
        RuntimeEventType::ToolCompleted => event
            .payload
            .get("summary")
            .and_then(Value::as_str)
            .filter(|summary| !summary.trim().is_empty())
            .map(|summary| format!("Tool: {}", compact_history_text(summary, 180))),
        RuntimeEventType::TaskCompleted => event
            .payload
            .get("status")
            .and_then(Value::as_str)
            .map(|status| format!("Task: {status}")),
        _ => None,
    }
}

fn compact_history_lines(lines: Vec<String>) -> String {
    const MAX_HISTORY_LINES: usize = 24;
    let start = lines.len().saturating_sub(MAX_HISTORY_LINES);
    lines[start..].join("\n")
}

fn compact_history_text(value: &str, max_chars: usize) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        compact
    } else {
        compact.chars().take(max_chars).collect::<String>()
    }
}

fn resolve_configured_provider(
    provider_env: Option<&golutra_config::ProviderRuntimeEnv>,
    mock: MockProvider,
) -> Result<ConfiguredProvider, ProviderError> {
    if let Some(provider_env) = provider_env {
        ConfiguredProvider::resolve_from_reader(mock, |key| provider_env.get(key))
    } else {
        ConfiguredProvider::resolve_from_env(mock)
    }
}

fn system_prompt() -> String {
    [
        "You are Golutra, a workspace coding agent.",
        "Use the provided tools whenever the task requires reading files, listing directories, searching, writing files, or running validation commands.",
        "Use workspace-relative paths. Do not invent file contents when a read or search tool can inspect them.",
        "For write tasks, call write_file or edit_file with complete arguments instead of only explaining the change.",
    ]
    .join(" ")
}

fn prompt_from_payload(payload: &Value) -> String {
    payload
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn string_payload(payload: &Value, key: &str, fallback: &str) -> String {
    non_empty_string_payload(payload, key).unwrap_or_else(|| fallback.to_owned())
}

fn non_empty_string_payload(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
}

fn title_from_payload(payload: &Value) -> String {
    let compact = compact_prompt(payload);
    if compact.is_empty() {
        "Untitled thread".to_owned()
    } else {
        compact.chars().take(80).collect()
    }
}

fn preview_from_payload(payload: &Value) -> String {
    compact_prompt(payload).chars().take(240).collect()
}

fn compact_event_summary(value: &str) -> String {
    compact_history_text(value, 160)
}

fn compact_prompt(payload: &Value) -> String {
    prompt_from_payload(payload)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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
    use std::{fs, path::Path};

    use golutra_config::{
        ProviderConfigPaths, ProviderConfigScope, ProviderInstallPlan, ProviderProfile,
    };
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
    async fn completed_task_allows_next_prompt_in_same_session() {
        let transport = InProcessTransport::in_memory().await.expect("transport");
        let session_id = SessionId::new();

        let first = transport
            .send_command(command(session_id, "hi"))
            .await
            .expect("first prompt");
        wait_for_task_completed_count(&transport, session_id, 1).await;
        let second = transport
            .send_command(command(session_id, "what next"))
            .await
            .expect("second prompt");
        let events = wait_for_task_completed_count(&transport, session_id, 2).await;

        assert!(first.accepted);
        assert!(second.accepted);
        assert!(
            second
                .reason
                .as_deref()
                .is_some_and(|reason| reason.starts_with("started task"))
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == RuntimeEventType::TaskCreated)
                .count(),
            2
        );
        assert!(
            events
                .iter()
                .all(|event| event.event_type != RuntimeEventType::BusyPolicyDecided)
        );
    }

    #[tokio::test]
    async fn workspace_transport_reuses_default_session_and_sqlite_events() {
        let workspace = tempdir().expect("workspace");
        install_workspace_mock_provider(workspace.path());
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
    async fn list_threads_hides_bootstrap_placeholder_thread() {
        let workspace = tempdir().expect("workspace");
        let transport = InProcessTransport::for_workspace(workspace.path())
            .await
            .expect("transport");

        let threads = transport.list_threads(10).await.expect("threads");

        assert!(threads.is_empty());
    }

    #[tokio::test]
    async fn workspace_transport_repairs_missing_default_thread_record() {
        let workspace = tempdir().expect("workspace");
        let golutra_dir = workspace.path().join(".golutra");
        fs::create_dir_all(&golutra_dir).expect("golutra dir");
        let stale_thread_id = ThreadId::new();
        let session_id = SessionId::new();
        fs::write(
            golutra_dir.join("default-thread"),
            stale_thread_id.to_string(),
        )
        .expect("default thread");
        fs::write(golutra_dir.join("default-session"), session_id.to_string())
            .expect("default session");

        let transport = InProcessTransport::for_workspace(workspace.path())
            .await
            .expect("transport repairs thread index");
        let thread = transport
            .resume_thread(transport.default_thread_id())
            .await
            .expect("default thread can resume after repair");

        assert_eq!(transport.default_thread_id(), stale_thread_id);
        assert_eq!(thread.session_id, session_id);
    }

    #[tokio::test]
    async fn workspace_transport_falls_back_to_latest_thread_when_pointer_is_stale() {
        let workspace = tempdir().expect("workspace");
        install_workspace_mock_provider(workspace.path());
        let first = InProcessTransport::for_workspace(workspace.path())
            .await
            .expect("first transport");
        let session_id = first.default_session_id();
        first
            .send_command(command(session_id, "list workspace"))
            .await
            .expect("command");
        wait_for_status(&first, session_id, TaskStatus::Completed).await;
        let original_thread_id = first.default_thread_id();
        fs::write(
            workspace.path().join(".golutra/default-thread"),
            ThreadId::new().to_string(),
        )
        .expect("stale default thread pointer");

        let repaired = InProcessTransport::for_workspace(workspace.path())
            .await
            .expect("transport repairs stale pointer");

        assert_eq!(repaired.default_thread_id(), original_thread_id);
        assert_eq!(repaired.default_session_id(), session_id);
        assert_eq!(
            fs::read_to_string(workspace.path().join(".golutra/default-thread"))
                .expect("default thread")
                .trim(),
            original_thread_id.to_string()
        );
    }

    #[tokio::test]
    async fn workspace_transport_does_not_repair_to_other_workspace_thread() {
        let workspace = tempdir().expect("workspace");
        let workspace_root = workspace
            .path()
            .canonicalize()
            .expect("workspace canonicalizes")
            .to_string_lossy()
            .to_string();
        let golutra_dir = workspace.path().join(".golutra");
        fs::create_dir_all(&golutra_dir).expect("golutra dir");
        let default_session_id = SessionId::new();
        let other_workspace_thread = ThreadRecord {
            thread_id: ThreadId::new(),
            session_id: SessionId::new(),
            parent_thread_id: None,
            workspace_root: Some("/tmp/other-golutra-workspace".to_owned()),
            title: "Other workspace".to_owned(),
            preview: "Do not resume from here".to_owned(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            recency_at: chrono::Utc::now(),
            archived: false,
        };
        fs::write(
            golutra_dir.join("default-thread"),
            other_workspace_thread.thread_id.to_string(),
        )
        .expect("default thread");
        fs::write(
            golutra_dir.join("default-session"),
            default_session_id.to_string(),
        )
        .expect("default session");
        let store = RuntimeStore::connect(&format!(
            "sqlite://{}",
            golutra_dir.join("runtime.sqlite").display()
        ))
        .await
        .expect("store");
        store
            .upsert_thread(&other_workspace_thread)
            .await
            .expect("other workspace thread");

        let transport = InProcessTransport::for_workspace(workspace.path())
            .await
            .expect("transport repairs current workspace only");
        let current_thread = transport
            .resume_thread(transport.default_thread_id())
            .await
            .expect("current workspace default thread resumes");
        let other_error = transport
            .resume_thread(other_workspace_thread.thread_id)
            .await
            .expect_err("other workspace thread is rejected");

        assert_ne!(
            transport.default_thread_id(),
            other_workspace_thread.thread_id
        );
        assert_eq!(
            current_thread.workspace_root.as_deref(),
            Some(workspace_root.as_str())
        );
        assert_eq!(current_thread.session_id, default_session_id);
        assert!(
            other_error
                .to_string()
                .contains("does not belong to workspace")
        );
    }

    #[tokio::test]
    async fn prompt_updates_resumed_thread_metadata_by_session() {
        let workspace = tempdir().expect("workspace");
        install_workspace_mock_provider(workspace.path());
        let transport = InProcessTransport::for_workspace(workspace.path())
            .await
            .expect("transport");
        let parent_thread_id = transport.default_thread_id();
        let child = transport
            .fork_thread(parent_thread_id)
            .await
            .expect("fork thread");

        transport
            .send_command(command_with_payload(
                child.session_id,
                json!({
                    "prompt": "write child output",
                    "path": "child.txt",
                    "content": "child",
                }),
            ))
            .await
            .expect("command");
        wait_for_status(&transport, child.session_id, TaskStatus::Completed).await;

        let threads = transport.list_threads(10).await.expect("threads");
        let child_after = threads
            .iter()
            .find(|thread| thread.thread_id == child.thread_id)
            .expect("child thread remains indexed");

        assert_eq!(child_after.preview, "write child output");
        assert_eq!(child_after.parent_thread_id, Some(parent_thread_id));
    }

    #[tokio::test]
    async fn prompt_updates_placeholder_thread_title_from_prompt() {
        let workspace = tempdir().expect("workspace");
        install_workspace_mock_provider(workspace.path());
        let transport = InProcessTransport::for_workspace(workspace.path())
            .await
            .expect("transport");
        let default_thread_id = transport.default_thread_id();

        transport
            .send_command(command_with_payload(
                transport.default_session_id(),
                json!({
                    "prompt": "write file chain.txt with content ok",
                }),
            ))
            .await
            .expect("command");
        wait_for_status(
            &transport,
            transport.default_session_id(),
            TaskStatus::Completed,
        )
        .await;

        let thread = transport
            .resume_thread(default_thread_id)
            .await
            .expect("default thread remains resumable");

        assert_eq!(thread.title, "write file chain.txt with content ok");
        assert_eq!(thread.preview, "write file chain.txt with content ok");
    }

    #[tokio::test]
    async fn resumed_session_context_includes_previous_conversation_summary() {
        let workspace = tempdir().expect("workspace");
        install_workspace_mock_provider(workspace.path());
        let transport = InProcessTransport::for_workspace(workspace.path())
            .await
            .expect("transport");

        transport
            .send_command(command_with_payload(
                transport.default_session_id(),
                json!({
                    "prompt": "write file first.txt with content done",
                }),
            ))
            .await
            .expect("command");
        wait_for_status(
            &transport,
            transport.default_session_id(),
            TaskStatus::Completed,
        )
        .await;

        let contributors = transport
            .host
            .context_contributors_for_task(
                transport.default_session_id(),
                TaskId::new(),
                "continue from previous task".to_owned(),
            )
            .await
            .expect("contributors");
        let history = contributors
            .iter()
            .find(|contributor| contributor.name == "conversation_history")
            .expect("history contributor");

        assert!(
            history
                .content
                .contains("User: write file first.txt with content done")
        );
        assert!(history.content.contains("Golutra: Completed: file written"));
        assert!(history.content.contains("Tool: file written"));
    }

    #[tokio::test]
    async fn prompt_with_explicit_thread_id_starts_new_thread_without_overwriting_default() {
        let workspace = tempdir().expect("workspace");
        install_workspace_mock_provider(workspace.path());
        let transport = InProcessTransport::for_workspace(workspace.path())
            .await
            .expect("transport");
        let default_thread_id = transport.default_thread_id();
        let default_session_id = transport.default_session_id();
        let tui_thread_id = ThreadId::new();
        let tui_session_id = SessionId::new();

        transport
            .send_command(command_with_payload(
                tui_session_id,
                json!({
                    "prompt": "write file tui.txt with content ok",
                    "_thread_id": tui_thread_id.to_string(),
                }),
            ))
            .await
            .expect("command");
        wait_for_status(&transport, tui_session_id, TaskStatus::Completed).await;
        let threads = transport.list_threads(10).await.expect("threads");
        let tui_thread = threads
            .iter()
            .find(|thread| thread.thread_id == tui_thread_id)
            .expect("tui thread indexed");
        let default_thread = transport
            .resume_thread(default_thread_id)
            .await
            .expect("default thread remains resumable");

        assert_eq!(tui_thread.session_id, tui_session_id);
        assert_eq!(tui_thread.preview, "write file tui.txt with content ok");
        assert_eq!(default_thread.session_id, default_session_id);
    }

    #[tokio::test]
    async fn prompt_runs_mock_agent_loop_and_writes_file() {
        let workspace = tempdir().expect("workspace");
        install_workspace_mock_provider(workspace.path());
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
        assert!(workspace.path().join(".golutra/checkpoints").exists());
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
    async fn prompt_plain_conversation_completes_without_tool_evidence() {
        let workspace = tempdir().expect("workspace");
        install_workspace_mock_provider(workspace.path());
        let transport = InProcessTransport::for_workspace(workspace.path())
            .await
            .expect("transport");
        let session_id = transport.default_session_id();

        let ack = transport
            .send_command(command(session_id, "你好"))
            .await
            .expect("command");
        let state = wait_for_status(&transport, session_id, TaskStatus::Completed).await;
        let projection = transport
            .query(RuntimeQuery {
                query_id: QueryId::new(),
                session_id,
                task_id: None,
                kind: RuntimeQueryKind::UserProjection,
                requester: ActorKind::Cli,
                cursor: None,
                timestamp: chrono::Utc::now(),
            })
            .await
            .expect("projection");

        assert!(ack.accepted);
        assert_eq!(projection_status(&state), Some(TaskStatus::Completed));
        assert_eq!(
            projection.get("final_message").and_then(Value::as_str),
            Some("mock provider completed without tool calls")
        );
    }

    #[test]
    fn plain_conversation_plan_does_not_send_workspace_tools() {
        let workspace = tempdir().expect("workspace");
        install_workspace_mock_provider(workspace.path());

        let plan = mock_provider_plan(Some(workspace.path()), &json!({"prompt": "你好"}), "你好")
            .expect("provider plan");

        assert!(!plan.touched_code);
        assert!(!plan.workspace_tools_enabled);
    }

    #[test]
    fn workspace_objective_plan_still_sends_workspace_tools() {
        let workspace = tempdir().expect("workspace");
        install_workspace_mock_provider(workspace.path());

        let plan = mock_provider_plan(
            Some(workspace.path()),
            &json!({"prompt": "读取 README.md"}),
            "读取 README.md",
        )
        .expect("provider plan");

        assert!(!plan.touched_code);
        assert!(plan.workspace_tools_enabled);
    }

    #[tokio::test]
    async fn prompt_write_file_natural_language_uses_requested_path_and_content() {
        let workspace = tempdir().expect("workspace");
        install_workspace_mock_provider(workspace.path());
        let transport = InProcessTransport::for_workspace(workspace.path())
            .await
            .expect("transport");
        let session_id = transport.default_session_id();

        let ack = transport
            .send_command(command(session_id, "write file smoke.txt with content ok"))
            .await
            .expect("command");
        let state = wait_for_status(&transport, session_id, TaskStatus::Completed).await;

        assert!(ack.accepted);
        assert_eq!(projection_status(&state), Some(TaskStatus::Completed));
        assert_eq!(
            fs::read_to_string(workspace.path().join("smoke.txt")).expect("file"),
            "ok"
        );
        assert!(!workspace.path().join("golutra-agent-output.txt").exists());
    }

    #[test]
    fn mock_write_file_args_prefers_payload_over_prompt() {
        let args = mock_write_file_args(
            &json!({
                "path": "explicit.txt",
                "content": "explicit",
            }),
            "write file prompt.txt with content prompt",
        );

        assert_eq!(
            args,
            MockWriteFileArgs {
                path: "explicit.txt".to_owned(),
                content: "explicit".to_owned(),
            }
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

    fn install_workspace_mock_provider(workspace_root: &Path) {
        let paths = ProviderConfigPaths::for_workspace(workspace_root).expect("provider paths");
        ProviderInstallPlan {
            scope: ProviderConfigScope::Workspace,
            profile: ProviderProfile::mock(),
            activate: true,
        }
        .apply(&paths)
        .expect("workspace mock provider");
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

    async fn wait_for_task_completed_count(
        transport: &InProcessTransport,
        session_id: SessionId,
        expected_count: usize,
    ) -> Vec<RuntimeEvent> {
        for _ in 0..40 {
            let event_values = transport
                .replay_events(EventFilter {
                    session_id,
                    task_id: None,
                    after_sequence_no: None,
                })
                .await
                .expect("events");
            let events = event_values
                .into_iter()
                .map(serde_json::from_value::<RuntimeEvent>)
                .collect::<Result<Vec<_>, _>>()
                .expect("typed events");
            let completed_count = events
                .iter()
                .filter(|event| event.event_type == RuntimeEventType::TaskCompleted)
                .count();
            if completed_count >= expected_count {
                return events;
            }
            sleep(Duration::from_millis(25)).await;
        }
        panic!("session did not record {expected_count} completed tasks");
    }
}
