//! Application-facing runtime services.
//!
//! The transport layer should only depend on these services.  `RuntimeHost`
//! owns execution and durable facts; this module owns the use-case boundary
//! that turns those facts into commands, queries, session operations and
//! governed trace reads.

use std::path::Path;
use std::sync::Arc;

pub use super::trace::TaskTraceService;
use super::{
    ClientError, EventFilter, EventPage, EventPageRequest, PostTaskCoordinator, RuntimeEvent,
    RuntimeEventStream, RuntimeExecutionOptions, RuntimeHost, RuntimeHostInfo, RuntimeQuery,
    SessionCommand, SessionPage, SessionPageRequest, SessionWindow, SessionWindowRequest,
    TaskTracePage, TaskTraceRequest, ThreadId, ThreadRebindResult, ThreadRecord, TurnId, Value,
};
use golutra_core::{PostTaskJob, SessionId, TaskId, WorkspaceId};
use golutra_protocol::{
    ArtifactChunk, ArtifactReadRequest, CommandAck, ContextProjection, EvaluationProjection,
};

/// The stable in-process application boundary for a governed runtime.
///
/// Frontends use this facade instead of reaching into `RuntimeHost`.  The
/// facade is deliberately transport-agnostic: `EmbeddedTransport`, the local
/// daemon and future IPC clients expose the same command/query semantics.
#[derive(Debug, Clone)]
pub struct RuntimeApplication {
    host: Arc<RuntimeHost>,
    commands: RuntimeCommandService,
    queries: RuntimeQueryService,
    sessions: RuntimeSessionService,
    trace: TaskTraceService,
    post_tasks: PostTaskCoordinator,
}

/// Canonical name used by the architecture docs for the governed runtime
/// application boundary.
pub type GovernedRuntime = RuntimeApplication;

impl RuntimeApplication {
    #[must_use]
    pub fn from_host(host: Arc<RuntimeHost>) -> Self {
        Self {
            commands: RuntimeCommandService::new(host.clone()),
            queries: RuntimeQueryService::new(host.clone()),
            sessions: RuntimeSessionService::new(host.clone()),
            trace: TaskTraceService::new(host.clone()),
            post_tasks: PostTaskCoordinator::for_host(host.clone()),
            host,
        }
    }

    pub async fn in_memory() -> Result<Self, ClientError> {
        Ok(Self::from_host(RuntimeHost::in_memory().await?))
    }

    pub async fn for_cwd(cwd: impl AsRef<Path>) -> Result<Self, ClientError> {
        Ok(Self::from_host(RuntimeHost::for_cwd(cwd).await?))
    }

    pub async fn for_cwd_with_options(
        cwd: impl AsRef<Path>,
        execution_options: RuntimeExecutionOptions,
    ) -> Result<Self, ClientError> {
        Ok(Self::from_host(
            RuntimeHost::for_cwd_with_options(cwd, execution_options).await?,
        ))
    }

    pub async fn ephemeral_for_cwd(cwd: impl AsRef<Path>) -> Result<Self, ClientError> {
        Ok(Self::from_host(RuntimeHost::ephemeral_for_cwd(cwd).await?))
    }

    pub async fn ephemeral_for_cwd_with_options(
        cwd: impl AsRef<Path>,
        execution_options: RuntimeExecutionOptions,
    ) -> Result<Self, ClientError> {
        Ok(Self::from_host(
            RuntimeHost::ephemeral_for_cwd_with_options(cwd, execution_options).await?,
        ))
    }

    pub async fn ephemeral_persistent_for_cwd(
        cwd: impl AsRef<Path>,
        state_home: impl AsRef<Path>,
    ) -> Result<Self, ClientError> {
        Ok(Self::from_host(
            RuntimeHost::ephemeral_persistent_for_cwd(cwd, state_home).await?,
        ))
    }

    pub async fn ephemeral_persistent_for_cwd_with_options(
        cwd: impl AsRef<Path>,
        state_home: impl AsRef<Path>,
        execution_options: RuntimeExecutionOptions,
    ) -> Result<Self, ClientError> {
        Ok(Self::from_host(
            RuntimeHost::ephemeral_persistent_for_cwd_with_options(
                cwd,
                state_home,
                execution_options,
            )
            .await?,
        ))
    }

    pub async fn from_home_and_cwd(
        home: impl AsRef<Path>,
        cwd: impl AsRef<Path>,
    ) -> Result<Self, ClientError> {
        Ok(Self::from_host(
            RuntimeHost::from_home_and_cwd(home, cwd).await?,
        ))
    }

    #[must_use]
    pub(crate) fn host(&self) -> &Arc<RuntimeHost> {
        &self.host
    }

    #[must_use]
    pub fn command_service(&self) -> &RuntimeCommandService {
        &self.commands
    }

    #[must_use]
    pub fn query_service(&self) -> &RuntimeQueryService {
        &self.queries
    }

    #[must_use]
    pub fn session_service(&self) -> &RuntimeSessionService {
        &self.sessions
    }

    #[must_use]
    pub fn trace_service(&self) -> &TaskTraceService {
        &self.trace
    }

    #[must_use]
    pub fn post_task_service(&self) -> &PostTaskCoordinator {
        &self.post_tasks
    }

    pub async fn send_command(&self, command: SessionCommand) -> Result<CommandAck, ClientError> {
        self.commands.execute(command).await
    }

    pub async fn query(&self, query: RuntimeQuery) -> Result<Value, ClientError> {
        self.queries.execute(query).await
    }

    pub async fn event_page(&self, request: EventPageRequest) -> Result<EventPage, ClientError> {
        self.queries.event_page(request).await
    }

    pub async fn replay_events(&self, filter: EventFilter) -> Result<Vec<Value>, ClientError> {
        self.queries.replay(filter).await
    }

    pub async fn subscribe(&self, filter: EventFilter) -> Result<RuntimeEventStream, ClientError> {
        self.queries.subscribe(filter).await
    }

    pub async fn task_trace(
        &self,
        request: TaskTraceRequest,
    ) -> Result<TaskTracePage, ClientError> {
        self.trace.read(request).await
    }

    pub async fn complete_task_trace(
        &self,
        request: TaskTraceRequest,
    ) -> Result<TaskTracePage, ClientError> {
        self.trace.read_complete(request).await
    }

    pub async fn read_artifact_chunk(
        &self,
        request: ArtifactReadRequest,
    ) -> Result<Option<ArtifactChunk>, ClientError> {
        self.trace.read_artifact(request).await
    }
}

/// Command use cases.  Idempotency, session validation and command journaling
/// remain inside the host so every transport gets exactly-once command
/// semantics at the application boundary.
#[derive(Debug, Clone)]
pub struct RuntimeCommandService {
    host: Arc<RuntimeHost>,
}

impl RuntimeCommandService {
    #[must_use]
    pub(crate) fn new(host: Arc<RuntimeHost>) -> Self {
        Self { host }
    }

    pub async fn execute(&self, command: SessionCommand) -> Result<CommandAck, ClientError> {
        Box::pin(self.host.clone().handle_command(command)).await
    }
}

/// Read-only runtime use cases.  These methods are the only query seam used
/// by normal frontends; they never mutate the lane or provider state.
#[derive(Debug, Clone)]
pub struct RuntimeQueryService {
    host: Arc<RuntimeHost>,
}

impl RuntimeQueryService {
    #[must_use]
    pub(crate) fn new(host: Arc<RuntimeHost>) -> Self {
        Self { host }
    }

    pub async fn execute(&self, query: RuntimeQuery) -> Result<Value, ClientError> {
        self.host.query(query).await
    }

    pub async fn event_page(&self, request: EventPageRequest) -> Result<EventPage, ClientError> {
        self.host.event_page(request).await
    }

    pub async fn replay(&self, filter: EventFilter) -> Result<Vec<Value>, ClientError> {
        self.host.replay_events(filter).await
    }

    pub async fn subscribe(&self, filter: EventFilter) -> Result<RuntimeEventStream, ClientError> {
        self.host.clone().event_stream(filter).await
    }

    #[must_use]
    pub fn subscribe_live(
        &self,
        filter: EventFilter,
    ) -> tokio::sync::broadcast::Receiver<RuntimeEvent> {
        self.host.subscribe_live(filter)
    }
}

/// Session/thread use cases.  Session identity and workspace ownership are
/// resolved here, rather than independently in each frontend.
#[derive(Debug, Clone)]
pub struct RuntimeSessionService {
    host: Arc<RuntimeHost>,
}

impl RuntimeSessionService {
    #[must_use]
    pub(crate) fn new(host: Arc<RuntimeHost>) -> Self {
        Self { host }
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
    pub fn workspace_id(&self) -> WorkspaceId {
        self.host.workspace_id()
    }

    #[must_use]
    pub fn cwd(&self) -> Option<&Path> {
        self.host.workspace_root()
    }

    pub async fn list_threads(&self, limit: u32) -> Result<Vec<ThreadRecord>, ClientError> {
        self.host.list_threads(limit).await
    }

    pub async fn session_page(
        &self,
        request: SessionPageRequest,
    ) -> Result<SessionPage, ClientError> {
        self.host.session_page(request).await
    }

    pub async fn session_window(
        &self,
        request: SessionWindowRequest,
    ) -> Result<SessionWindow, ClientError> {
        self.host.session_window(request).await
    }

    pub async fn thread_for_session(
        &self,
        session_id: SessionId,
    ) -> Result<Option<ThreadRecord>, ClientError> {
        self.host.thread_for_session(session_id).await
    }

    pub async fn resume_thread(&self, thread_id: ThreadId) -> Result<ThreadRecord, ClientError> {
        self.host.resume_thread(thread_id).await
    }

    pub async fn fork_thread(
        &self,
        thread_id: ThreadId,
        from_turn_id: Option<TurnId>,
    ) -> Result<ThreadRecord, ClientError> {
        self.host.fork_thread(thread_id, from_turn_id).await
    }

    pub async fn export_thread_rollout(
        &self,
        thread_id: ThreadId,
    ) -> Result<super::RolloutExport, ClientError> {
        self.host.export_thread_rollout(thread_id).await
    }

    pub async fn rebind_thread(
        &self,
        thread_id: ThreadId,
        from_workspace_root: impl AsRef<Path>,
    ) -> Result<ThreadRebindResult, ClientError> {
        self.host
            .rebind_thread(thread_id, from_workspace_root)
            .await
    }

    pub async fn recover_orphaned_tasks(&self) -> Result<usize, ClientError> {
        self.host.recover_orphaned_tasks().await
    }

    pub async fn runtime_info(
        &self,
        base_url: impl Into<String>,
    ) -> Result<RuntimeHostInfo, ClientError> {
        self.host.runtime_info(base_url).await
    }
}

/// 治理用例入口。写操作继续经过 command journal，读取则返回类型化
/// context/evaluation projection，避免调用方解析内部 store 或事件文本。
#[derive(Debug, Clone)]
pub struct RuntimeGovernanceService {
    host: Arc<RuntimeHost>,
    commands: RuntimeCommandService,
    trace: TaskTraceService,
    post_tasks: PostTaskCoordinator,
}

impl RuntimeGovernanceService {
    #[must_use]
    pub(crate) fn new(application: &RuntimeApplication) -> Self {
        Self {
            host: application.host.clone(),
            commands: application.commands.clone(),
            trace: application.trace.clone(),
            post_tasks: application.post_tasks.clone(),
        }
    }

    pub async fn execute(&self, command: SessionCommand) -> Result<CommandAck, ClientError> {
        self.commands.execute(command).await
    }

    pub async fn trace(&self, request: TaskTraceRequest) -> Result<TaskTracePage, ClientError> {
        self.trace.read(request).await
    }

    pub async fn complete_trace(
        &self,
        request: TaskTraceRequest,
    ) -> Result<TaskTracePage, ClientError> {
        self.trace.read_complete(request).await
    }

    pub async fn context_projection(
        &self,
        session_id: SessionId,
        task_id: TaskId,
    ) -> Result<ContextProjection, ClientError> {
        self.host
            .ensure_task_in_session(session_id, task_id)
            .await?;
        self.host
            .storage
            .governance
            .context_projection(session_id, task_id)
            .await
    }

    pub async fn evaluation_projection(
        &self,
        session_id: SessionId,
        task_id: TaskId,
    ) -> Result<EvaluationProjection, ClientError> {
        self.host
            .ensure_task_in_session(session_id, task_id)
            .await?;
        self.host
            .storage
            .governance
            .evaluation_projection(session_id, task_id)
            .await
    }

    pub async fn wait_for_evaluation(
        &self,
        task_id: TaskId,
    ) -> Result<Option<PostTaskJob>, ClientError> {
        self.post_tasks.wait_for_terminal(task_id).await
    }
}

impl RuntimeApplication {
    #[must_use]
    pub fn governance_service(&self) -> RuntimeGovernanceService {
        RuntimeGovernanceService::new(self)
    }
}
