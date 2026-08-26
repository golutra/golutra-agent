//! Runtime transports and the HTTP/SSE protocol boundary.

use async_trait::async_trait;
use futures_util::StreamExt;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, Ordering},
};

use super::*;
use crate::RuntimeApplication;
use golutra_protocol::{decode_event, encode_command_value_for_protocol};
use tokio_util::sync::CancellationToken;

#[cfg(unix)]
mod ipc;
#[cfg(unix)]
pub use ipc::UnixIpcTransport;
#[path = "transport_operation.rs"]
mod transport_operation;
pub use transport_operation::{RuntimeOperation, RuntimeOperationResult};

const MAX_HTTP_ERROR_MESSAGE_BYTES: usize = 4 * 1024;
const SUBSCRIPTION_RESPONSE_HEAD_TIMEOUT: Duration = Duration::from_secs(10);

#[async_trait]
pub trait RuntimeClient {
    async fn send_command(&self, command: SessionCommand) -> Result<CommandAck, ClientError>;
    async fn query(&self, query: RuntimeQuery) -> Result<Value, ClientError>;
    async fn event_page(&self, request: EventPageRequest) -> Result<EventPage, ClientError>;
    async fn replay_events(&self, filter: EventFilter) -> Result<Vec<Value>, ClientError>;
    async fn subscribe(&self, filter: EventFilter) -> Result<RuntimeEventStream, ClientError>;
}

#[async_trait]
pub trait TaskTraceClient {
    async fn task_trace(&self, request: TaskTraceRequest) -> Result<TaskTracePage, ClientError>;

    async fn complete_task_trace(
        &self,
        request: TaskTraceRequest,
    ) -> Result<TaskTracePage, ClientError> {
        crate::trace::read_complete_trace(request, |request| self.task_trace(request)).await
    }

    async fn read_artifact_chunk(
        &self,
        request: ArtifactReadRequest,
    ) -> Result<Option<ArtifactChunk>, ClientError>;
}

#[async_trait]
pub trait RuntimeOperationClient: RuntimeClient + TaskTraceClient {
    /// Dispatch one typed runtime operation after the adapter has resolved its
    /// framing and authentication details.
    async fn execute_operation(
        &self,
        operation: RuntimeOperation,
    ) -> Result<RuntimeOperationResult, ClientError> {
        match operation {
            RuntimeOperation::SendCommand(command) => self
                .send_command(command)
                .await
                .map(RuntimeOperationResult::CommandAck),
            RuntimeOperation::Query(query) => {
                self.query(query).await.map(RuntimeOperationResult::Query)
            }
            RuntimeOperation::EventPage(request) => self
                .event_page(request)
                .await
                .map(RuntimeOperationResult::EventPage),
            RuntimeOperation::ReplayEvents(filter) => self
                .replay_events(filter)
                .await
                .map(RuntimeOperationResult::ReplayEvents),
            RuntimeOperation::Subscribe(filter) => self
                .subscribe(filter)
                .await
                .map(RuntimeOperationResult::Subscription),
            RuntimeOperation::TaskTrace(request) => self
                .task_trace(request)
                .await
                .map(|trace| RuntimeOperationResult::TaskTrace(Box::new(trace))),
            RuntimeOperation::ReadArtifactChunk(request) => self
                .read_artifact_chunk(request)
                .await
                .map(RuntimeOperationResult::ArtifactChunk),
        }
    }
}

impl<T> RuntimeOperationClient for T where T: RuntimeClient + TaskTraceClient + ?Sized {}

/// Shared lifecycle state for transports whose background subscriptions outlive
/// the request that created them. Closing the owner must cancel reconnect loops
/// as well as revoke the server-side attachment.
#[derive(Debug, Clone)]
pub(crate) struct TransportLifecycle {
    closed: Arc<AtomicBool>,
    cancellation: CancellationToken,
}

impl Default for TransportLifecycle {
    fn default() -> Self {
        Self {
            closed: Arc::new(AtomicBool::new(false)),
            cancellation: CancellationToken::new(),
        }
    }
}

impl TransportLifecycle {
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn close(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            self.cancellation.cancel();
        }
    }

    fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }
}

#[derive(Debug)]
pub struct RuntimeEventStream {
    receiver: mpsc::Receiver<Result<RuntimeEvent, ClientError>>,
}

impl RuntimeEventStream {
    #[must_use]
    pub fn new(receiver: mpsc::Receiver<Result<RuntimeEvent, ClientError>>) -> Self {
        Self { receiver }
    }

    pub async fn recv(&mut self) -> Option<Result<RuntimeEvent, ClientError>> {
        self.receiver.recv().await
    }

    pub fn try_recv(
        &mut self,
    ) -> Result<Result<RuntimeEvent, ClientError>, mpsc::error::TryRecvError> {
        self.receiver.try_recv()
    }
}

#[derive(Debug, Clone)]
pub struct EmbeddedTransport {
    #[allow(dead_code)]
    pub(crate) host: Arc<RuntimeHost>,
    application: RuntimeApplication,
}

impl EmbeddedTransport {
    #[must_use]
    pub fn new(host: Arc<RuntimeHost>) -> Self {
        let application = RuntimeApplication::from_host(host.clone());
        Self { host, application }
    }

    #[must_use]
    pub fn from_application(application: RuntimeApplication) -> Self {
        let host = application.host().clone();
        Self { host, application }
    }

    pub async fn in_memory() -> Result<Self, ClientError> {
        Ok(Self::new(RuntimeHost::in_memory().await?))
    }

    pub async fn ephemeral_for_cwd(cwd: impl AsRef<Path>) -> Result<Self, ClientError> {
        Ok(Self::new(RuntimeHost::ephemeral_for_cwd(cwd).await?))
    }

    pub async fn ephemeral_for_cwd_with_options(
        cwd: impl AsRef<Path>,
        execution_options: RuntimeExecutionOptions,
    ) -> Result<Self, ClientError> {
        Ok(Self::new(
            RuntimeHost::ephemeral_for_cwd_with_options(cwd, execution_options).await?,
        ))
    }

    pub async fn ephemeral_persistent_for_cwd(
        cwd: impl AsRef<Path>,
        state_home: impl AsRef<Path>,
    ) -> Result<Self, ClientError> {
        Ok(Self::new(
            RuntimeHost::ephemeral_persistent_for_cwd(cwd, state_home).await?,
        ))
    }

    pub async fn ephemeral_persistent_for_cwd_with_options(
        cwd: impl AsRef<Path>,
        state_home: impl AsRef<Path>,
        execution_options: RuntimeExecutionOptions,
    ) -> Result<Self, ClientError> {
        Ok(Self::new(
            RuntimeHost::ephemeral_persistent_for_cwd_with_options(
                cwd,
                state_home,
                execution_options,
            )
            .await?,
        ))
    }

    pub async fn open_persisted_run(run_root: impl AsRef<Path>) -> Result<Self, ClientError> {
        Ok(Self::new(RuntimeHost::open_persisted_run(run_root).await?))
    }

    pub async fn for_current_cwd() -> Result<Self, ClientError> {
        let cwd = std::env::current_dir().map_err(|error| ClientError::Io(error.to_string()))?;
        Self::for_cwd(cwd).await
    }

    pub async fn for_cwd(cwd: impl AsRef<Path>) -> Result<Self, ClientError> {
        Ok(Self::new(RuntimeHost::for_cwd(cwd).await?))
    }

    pub async fn for_cwd_with_options(
        cwd: impl AsRef<Path>,
        execution_options: RuntimeExecutionOptions,
    ) -> Result<Self, ClientError> {
        Ok(Self::new(
            RuntimeHost::for_cwd_with_options(cwd, execution_options).await?,
        ))
    }

    pub async fn from_home_and_cwd(
        home: impl AsRef<Path>,
        cwd: impl AsRef<Path>,
    ) -> Result<Self, ClientError> {
        Ok(Self::new(RuntimeHost::from_home_and_cwd(home, cwd).await?))
    }

    #[must_use]
    pub fn default_session_id(&self) -> SessionId {
        self.application.session_service().default_session_id()
    }

    #[must_use]
    pub fn default_thread_id(&self) -> ThreadId {
        self.application.session_service().default_thread_id()
    }

    #[must_use]
    pub fn cwd(&self) -> Option<&Path> {
        self.application.session_service().cwd()
    }

    #[must_use]
    pub fn workspace_id(&self) -> WorkspaceId {
        self.application.session_service().workspace_id()
    }

    #[must_use]
    pub fn subscribe_live(&self, filter: EventFilter) -> broadcast::Receiver<RuntimeEvent> {
        self.application.query_service().subscribe_live(filter)
    }

    pub async fn list_threads(&self, limit: u32) -> Result<Vec<ThreadRecord>, ClientError> {
        self.application.session_service().list_threads(limit).await
    }

    pub async fn session_page(
        &self,
        request: SessionPageRequest,
    ) -> Result<SessionPage, ClientError> {
        self.application
            .session_service()
            .session_page(request)
            .await
    }

    pub async fn session_window(
        &self,
        request: SessionWindowRequest,
    ) -> Result<SessionWindow, ClientError> {
        self.application
            .session_service()
            .session_window(request)
            .await
    }

    pub async fn thread_for_session(
        &self,
        session_id: SessionId,
    ) -> Result<Option<ThreadRecord>, ClientError> {
        self.application
            .session_service()
            .thread_for_session(session_id)
            .await
    }

    pub async fn resume_thread(&self, thread_id: ThreadId) -> Result<ThreadRecord, ClientError> {
        self.application
            .session_service()
            .resume_thread(thread_id)
            .await
    }

    pub async fn fork_thread(
        &self,
        thread_id: ThreadId,
        from_turn_id: Option<TurnId>,
    ) -> Result<ThreadRecord, ClientError> {
        self.application
            .session_service()
            .fork_thread(thread_id, from_turn_id)
            .await
    }

    pub async fn export_thread_rollout(
        &self,
        thread_id: ThreadId,
    ) -> Result<RolloutExport, ClientError> {
        self.application
            .session_service()
            .export_thread_rollout(thread_id)
            .await
    }

    pub async fn rebind_thread(
        &self,
        thread_id: ThreadId,
        from_workspace_root: impl AsRef<Path>,
    ) -> Result<ThreadRebindResult, ClientError> {
        self.application
            .session_service()
            .rebind_thread(thread_id, from_workspace_root)
            .await
    }

    pub async fn recover_orphaned_tasks(&self) -> Result<usize, ClientError> {
        self.application
            .session_service()
            .recover_orphaned_tasks()
            .await
    }

    pub async fn runtime_info(
        &self,
        base_url: impl Into<String>,
    ) -> Result<RuntimeHostInfo, ClientError> {
        self.application
            .session_service()
            .runtime_info(base_url)
            .await
    }

    /// Whether the host still has process-local work that a later attachment
    /// must be able to observe or cancel.
    pub async fn has_active_work(&self) -> bool {
        if self
            .host
            .execution
            .post_task_schedule_pending
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0
        {
            return true;
        }
        if !self.host.execution.task_controls.lock().await.is_empty() {
            return true;
        }
        if self
            .host
            .execution
            .delegation_operations
            .lock()
            .await
            .values()
            .any(|operation| !operation.is_complete())
        {
            return true;
        }
        if self
            .host
            .storage
            .repositories
            .jobs
            .has_nonterminal_for_workspace(&self.host.workspace_id.to_string())
            .await
            .unwrap_or(true)
        {
            return true;
        }
        self.host
            .execution
            .process_supervisor
            .has_running_processes()
            .await
    }

    /// Shut down process work owned by this embedded runtime while the async
    /// executor is still available for supervisor terminal bookkeeping.
    pub async fn close(&self) -> Result<(), ClientError> {
        self.host.close().await
    }
}

#[async_trait]
impl RuntimeClient for EmbeddedTransport {
    async fn send_command(&self, command: SessionCommand) -> Result<CommandAck, ClientError> {
        self.application.send_command(command).await
    }

    async fn query(&self, query: RuntimeQuery) -> Result<Value, ClientError> {
        self.application.query(query).await
    }

    async fn event_page(&self, request: EventPageRequest) -> Result<EventPage, ClientError> {
        self.application.event_page(request).await
    }

    async fn replay_events(&self, filter: EventFilter) -> Result<Vec<Value>, ClientError> {
        self.application.replay_events(filter).await
    }

    async fn subscribe(&self, filter: EventFilter) -> Result<RuntimeEventStream, ClientError> {
        self.application.subscribe(filter).await
    }
}

#[async_trait]
impl TaskTraceClient for EmbeddedTransport {
    async fn task_trace(&self, request: TaskTraceRequest) -> Result<TaskTracePage, ClientError> {
        self.application.task_trace(request).await
    }

    async fn read_artifact_chunk(
        &self,
        request: ArtifactReadRequest,
    ) -> Result<Option<ArtifactChunk>, ClientError> {
        self.application.read_artifact_chunk(request).await
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeHostInfo {
    pub instance_id: String,
    pub pid: u32,
    pub base_url: String,
    pub cwd: String,
    pub workspace_id: WorkspaceId,
    pub default_session_id: SessionId,
    pub default_thread_id: ThreadId,
    pub started_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppServerInfo {
    pub instance_id: String,
    pub pid: u32,
    pub base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipc_path: Option<String>,
    pub protocol_versions: ProtocolVersionRange,
    pub started_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeAttachment {
    pub attachment_id: String,
    pub runtime: RuntimeHostInfo,
}

#[derive(Debug, Clone)]
pub struct HttpSseTransport {
    pub(crate) client: reqwest::Client,
    pub(crate) base_url: String,
    pub(crate) server_info: AppServerInfo,
    pub(crate) protocol_version: u32,
    pub(crate) info: RuntimeHostInfo,
    pub(crate) cwd: PathBuf,
    pub(crate) attachment_id: Arc<RwLock<String>>,
    pub(crate) refresh_lock: Arc<tokio::sync::Mutex<()>>,
    pub(crate) lifecycle: TransportLifecycle,
    pub(crate) transport_token: Arc<SecretString>,
}

impl HttpSseTransport {
    pub async fn connect(
        base_url: impl Into<String>,
        cwd: impl AsRef<Path>,
    ) -> Result<Self, ClientError> {
        let token = std::env::var(APP_SERVER_TRANSPORT_TOKEN_ENV).map_err(|_| {
            ClientError::Http(format!(
                "remote runtime requires {APP_SERVER_TRANSPORT_TOKEN_ENV}"
            ))
        })?;
        Self::connect_with_token(base_url, cwd, SecretString::from(token)).await
    }

    pub async fn connect_with_token(
        base_url: impl Into<String>,
        cwd: impl AsRef<Path>,
        transport_token: SecretString,
    ) -> Result<Self, ClientError> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .build()
            .map_err(|error| ClientError::Http(error.to_string()))?;
        let base_url = base_url.into().trim_end_matches('/').to_owned();
        validate_remote_app_server_base_url(&base_url)?;
        validate_transport_token(transport_token.expose_secret())?;
        let requested_cwd = cwd.as_ref().to_path_buf();
        if !requested_cwd.is_absolute() {
            return Err(ClientError::Http(format!(
                "remote runtime cwd must be absolute: {}",
                requested_cwd.display()
            )));
        }
        let response = authenticated_request(
            client.get(format!("{base_url}/runtime/info")),
            &transport_token,
            RUNTIME_PROTOCOL_VERSION,
        )
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|error| ClientError::Http(error.to_string()))?;
        let server_info: AppServerInfo = decode_http_response(response).await?;
        let protocol_version = ProtocolVersionRange::runtime()
            .highest_common(server_info.protocol_versions)
            .ok_or_else(|| {
                ClientError::Http(format!(
                    "runtime protocol ranges have no common version: client {}..={}, server {}..={}",
                    ProtocolVersionRange::runtime().minimum,
                    ProtocolVersionRange::runtime().current,
                    server_info.protocol_versions.minimum,
                    server_info.protocol_versions.current,
                ))
            })?;
        let response = authenticated_request(
            client.post(format!("{base_url}/runtime/attach")),
            &transport_token,
            protocol_version,
        )
        .json(&json!({
            "cwd": requested_cwd,
            "protocol_version": protocol_version,
        }))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|error| ClientError::Http(error.to_string()))?;
        let attachment: RuntimeAttachment = decode_http_response(response).await?;
        let attached_cwd = PathBuf::from(&attachment.runtime.cwd);
        let transport = Self {
            client,
            base_url,
            server_info,
            protocol_version,
            info: attachment.runtime,
            cwd: attached_cwd,
            attachment_id: Arc::new(RwLock::new(attachment.attachment_id)),
            refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
            lifecycle: TransportLifecycle::default(),
            transport_token: Arc::new(transport_token),
        };
        if !transport.cwd.is_absolute() {
            let error = ClientError::Protocol(format!(
                "runtime returned a non-absolute cwd: {}",
                transport.cwd.display()
            ));
            let _ = transport.close().await;
            return Err(error);
        }
        Ok(transport)
    }

    pub async fn connect_local_daemon(cwd: impl AsRef<Path>) -> Result<Self, ClientError> {
        let paths = RuntimePaths::for_cwd(cwd.as_ref())?;
        let endpoint_path = paths.app_server_endpoint;
        let bytes = tokio::fs::read(&endpoint_path).await.map_err(|error| {
            ClientError::Daemon(format!("{}: {error}", endpoint_path.display()))
        })?;
        let endpoint: AppServerInfo = serde_json::from_slice(&bytes)?;
        validate_local_app_server_base_url(&endpoint.base_url)?;
        let transport_token = read_transport_token(&paths.app_server_transport_token).await?;
        let transport =
            Self::connect_with_token(&endpoint.base_url, &paths.cwd, transport_token).await?;
        if transport.server_info.instance_id != endpoint.instance_id {
            let error = ClientError::Daemon(
                "app-server endpoint metadata does not match the running server".to_owned(),
            );
            let _ = transport.close().await;
            return Err(error);
        }
        if transport.cwd != paths.cwd {
            let error = ClientError::Daemon(format!(
                "app-server attached `{}` instead of local cwd `{}`",
                transport.cwd.display(),
                paths.cwd.display()
            ));
            let _ = transport.close().await;
            return Err(error);
        }
        Ok(transport)
    }

    #[must_use]
    pub fn info(&self) -> &RuntimeHostInfo {
        &self.info
    }

    #[must_use]
    pub fn server_info(&self) -> &AppServerInfo {
        &self.server_info
    }

    pub async fn list_threads(&self, limit: u32) -> Result<Vec<ThreadRecord>, ClientError> {
        let response = self
            .send_attached(|attachment_id| {
                self.authenticated(self.client.get(self.url("/threads")))
                    .header(APP_SERVER_ATTACHMENT_HEADER, attachment_id)
                    .query(&[("limit", limit)])
                    .timeout(Duration::from_secs(30))
            })
            .await?;
        decode_http_response(response).await
    }

    pub async fn session_page(
        &self,
        request: SessionPageRequest,
    ) -> Result<SessionPage, ClientError> {
        let response = self
            .send_attached(|attachment_id| {
                self.authenticated(self.client.post(self.url("/sessions/page")))
                    .header(APP_SERVER_ATTACHMENT_HEADER, attachment_id)
                    .json(&request)
                    .timeout(Duration::from_secs(30))
            })
            .await?;
        decode_http_response(response).await
    }

    pub async fn session_window(
        &self,
        request: SessionWindowRequest,
    ) -> Result<SessionWindow, ClientError> {
        let response = self
            .send_attached(|attachment_id| {
                self.authenticated(self.client.post(self.url("/sessions/window")))
                    .header(APP_SERVER_ATTACHMENT_HEADER, attachment_id)
                    .json(&request)
                    .timeout(Duration::from_secs(30))
            })
            .await?;
        decode_http_response(response).await
    }

    pub async fn thread_for_session(
        &self,
        session_id: SessionId,
    ) -> Result<Option<ThreadRecord>, ClientError> {
        let response = self
            .send_attached(|attachment_id| {
                self.authenticated(
                    self.client
                        .get(self.url(&format!("/sessions/{session_id}/thread"))),
                )
                .header(APP_SERVER_ATTACHMENT_HEADER, attachment_id)
                .timeout(Duration::from_secs(30))
            })
            .await?;
        decode_http_response(response).await
    }

    pub async fn resume_thread(&self, thread_id: ThreadId) -> Result<ThreadRecord, ClientError> {
        let response = self
            .send_attached(|attachment_id| {
                self.authenticated(
                    self.client
                        .post(self.url(&format!("/threads/{thread_id}/resume"))),
                )
                .header(APP_SERVER_ATTACHMENT_HEADER, attachment_id)
                .timeout(Duration::from_secs(30))
            })
            .await?;
        decode_http_response(response).await
    }

    pub async fn fork_thread(
        &self,
        thread_id: ThreadId,
        from_turn_id: Option<TurnId>,
    ) -> Result<ThreadRecord, ClientError> {
        let response = self
            .send_attached(|attachment_id| {
                self.authenticated(
                    self.client
                        .post(self.url(&format!("/threads/{thread_id}/fork"))),
                )
                .header(APP_SERVER_ATTACHMENT_HEADER, attachment_id)
                .json(&json!({"from_turn_id": from_turn_id}))
                .timeout(Duration::from_secs(30))
            })
            .await?;
        decode_http_response(response).await
    }

    pub async fn export_thread_rollout(
        &self,
        thread_id: ThreadId,
    ) -> Result<RolloutExport, ClientError> {
        let response = self
            .send_attached(|attachment_id| {
                self.authenticated(
                    self.client
                        .post(self.url(&format!("/threads/{thread_id}/rollout/export"))),
                )
                .header(APP_SERVER_ATTACHMENT_HEADER, attachment_id)
                .timeout(Duration::from_secs(30))
            })
            .await?;
        decode_http_response(response).await
    }

    pub async fn rebind_thread(
        &self,
        thread_id: ThreadId,
        from_workspace_root: impl AsRef<Path>,
    ) -> Result<ThreadRebindResult, ClientError> {
        let from_workspace_root = from_workspace_root.as_ref().display().to_string();
        let response = self
            .send_attached(|attachment_id| {
                self.authenticated(
                    self.client
                        .post(self.url(&format!("/threads/{thread_id}/rebind"))),
                )
                .header(APP_SERVER_ATTACHMENT_HEADER, attachment_id)
                .json(&json!({"from_workspace_root": from_workspace_root}))
                .timeout(Duration::from_secs(30))
            })
            .await?;
        decode_http_response(response).await
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn authenticated(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        authenticated_request(request, &self.transport_token, self.protocol_version)
    }

    fn current_attachment_id(&self) -> Result<String, ClientError> {
        self.attachment_id
            .read()
            .map(|attachment_id| attachment_id.clone())
            .map_err(|_| ClientError::Http("runtime attachment lock is poisoned".to_owned()))
    }

    pub(crate) fn current_attachment_actor_id(&self) -> Result<String, ClientError> {
        self.ensure_open()?;
        self.current_attachment_id()
            .map(|id| app_server_attachment_actor_id(&id))
    }

    fn ensure_open(&self) -> Result<(), ClientError> {
        if self.lifecycle.is_closed() {
            return Err(ClientError::Http("runtime transport is closed".to_owned()));
        }
        Ok(())
    }

    /// Explicitly release the server-side attachment capability. Network clients cannot rely
    /// on `Drop` to perform an asynchronous request, so owners should call this when their
    /// connection or application lifetime ends. Local shutdown is idempotent, while a failed
    /// remote detach remains retryable until the server confirms that the capability is gone.
    pub async fn close(&self) -> Result<(), ClientError> {
        let _refresh_guard = self.refresh_lock.lock().await;
        let attachment_id = self.current_attachment_id()?;
        // Stop local reconnect loops before the best-effort remote detach. A daemon restart or
        // socket failure must not leave a cloned subscription retrying after close() returns. We
        // intentionally retain the ID below so a later close() can retry the remote operation.
        self.lifecycle.close();
        if attachment_id.is_empty() {
            return Ok(());
        }
        let response = self
            .authenticated(
                self.client
                    .delete(self.url(&format!("/runtime/attach/{attachment_id}"))),
            )
            .header(APP_SERVER_ATTACHMENT_HEADER, &attachment_id)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|error| ClientError::Http(error.to_string()))?;
        if response.status().is_success()
            || matches!(
                response.status(),
                reqwest::StatusCode::GONE | reqwest::StatusCode::NOT_FOUND
            )
        {
            *self.attachment_id.write().map_err(|_| {
                ClientError::Http("runtime attachment lock is poisoned".to_owned())
            })? = String::new();
            return Ok(());
        }
        let (status, body) = read_bounded_http_body(response).await?;
        Err(http_status_error(
            status,
            &body,
            "runtime attachment close failed",
        ))
    }

    async fn refresh_attachment(&self, stale_attachment_id: &str) -> Result<String, ClientError> {
        let _refresh_guard = self.refresh_lock.lock().await;
        self.ensure_open()?;
        let current = self.current_attachment_id()?;
        if current.is_empty() {
            return Err(ClientError::Http(
                "runtime transport has no attachment".to_owned(),
            ));
        }
        if current != stale_attachment_id {
            return Ok(current);
        }
        let response = self
            .authenticated(self.client.post(self.url("/runtime/attach")))
            .json(&json!({
                "cwd": self.cwd,
                "protocol_version": self.protocol_version,
            }))
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|error| ClientError::Http(error.to_string()))?;
        let attachment: RuntimeAttachment = decode_http_response(response).await?;
        let attached_cwd = PathBuf::from(&attachment.runtime.cwd);
        if attached_cwd != self.cwd {
            let attachment_id = attachment.attachment_id;
            let _ = self
                .authenticated(
                    self.client
                        .delete(self.url(&format!("/runtime/attach/{attachment_id}"))),
                )
                .header(APP_SERVER_ATTACHMENT_HEADER, &attachment_id)
                .timeout(Duration::from_secs(30))
                .send()
                .await;
            return Err(ClientError::Http(format!(
                "runtime reattached `{}` instead of requested cwd `{}`",
                attached_cwd.display(),
                self.cwd.display()
            )));
        }
        let attachment_id = attachment.attachment_id;
        *self
            .attachment_id
            .write()
            .map_err(|_| ClientError::Http("runtime attachment lock is poisoned".to_owned()))? =
            attachment_id.clone();
        // Revoke the stale capability only after the replacement is active so concurrent
        // refreshes cannot accumulate unreachable control capabilities.
        let _ = self
            .authenticated(
                self.client
                    .delete(self.url(&format!("/runtime/attach/{stale_attachment_id}"))),
            )
            .header(APP_SERVER_ATTACHMENT_HEADER, stale_attachment_id)
            .timeout(Duration::from_secs(30))
            .send()
            .await;
        Ok(attachment_id)
    }

    async fn send_attached<F>(&self, build: F) -> Result<reqwest::Response, ClientError>
    where
        F: Fn(&str) -> reqwest::RequestBuilder,
    {
        self.ensure_open()?;
        let attachment_id = self.current_attachment_id()?;
        if attachment_id.is_empty() {
            return Err(ClientError::Http(
                "runtime transport has no attachment".to_owned(),
            ));
        }
        let response = build(&attachment_id)
            .send()
            .await
            .map_err(|error| ClientError::Http(error.to_string()))?;
        if response.status() != reqwest::StatusCode::GONE {
            return Ok(response);
        }
        let attachment_id = self.refresh_attachment(&attachment_id).await?;
        build(&attachment_id)
            .send()
            .await
            .map_err(|error| ClientError::Http(error.to_string()))
    }

    async fn send_attached_for_subscription<F>(
        &self,
        cancellation: &CancellationToken,
        build: F,
    ) -> Result<reqwest::Response, ClientError>
    where
        F: Fn(&str) -> reqwest::RequestBuilder,
    {
        // A live SSE request intentionally has no request-wide timeout. Bound only the response
        // head, and let the shared lifecycle cancel the request after the subscription owner is
        // closed.
        let result = tokio::select! {
            _ = cancellation.cancelled() => {
                Err(ClientError::Http("runtime transport is closed".to_owned()))
            }
            result = tokio::time::timeout(
                SUBSCRIPTION_RESPONSE_HEAD_TIMEOUT,
                self.send_attached(build),
            ) => result.map_err(|_| {
                ClientError::Http("SSE response head timed out".to_owned())
            }),
        };
        result?
    }
}

fn authenticated_request(
    request: reqwest::RequestBuilder,
    transport_token: &SecretString,
    protocol_version: u32,
) -> reqwest::RequestBuilder {
    request
        .bearer_auth(transport_token.expose_secret())
        .header(APP_SERVER_PROTOCOL_HEADER, protocol_version)
}

fn validate_transport_token(token: &str) -> Result<(), ClientError> {
    if token.len() < 32 || token.len() > 512 || token.chars().any(char::is_whitespace) {
        return Err(ClientError::Http(
            "runtime transport token must contain 32..=512 non-whitespace characters".to_owned(),
        ));
    }
    Ok(())
}

async fn read_transport_token(path: &Path) -> Result<SecretString, ClientError> {
    let metadata = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|error| ClientError::Daemon(format!("{}: {error}", path.display())))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 1024 {
        return Err(ClientError::Daemon(format!(
            "app-server transport token path is not a bounded regular file: {}",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(ClientError::Daemon(format!(
                "app-server transport token must be owner-only: {}",
                path.display()
            )));
        }
    }
    let token = tokio::fs::read_to_string(path)
        .await
        .map_err(|error| ClientError::Daemon(format!("{}: {error}", path.display())))?;
    let token = token.trim().to_owned();
    validate_transport_token(&token)?;
    Ok(SecretString::from(token))
}

pub(crate) fn validate_remote_app_server_base_url(base_url: &str) -> Result<(), ClientError> {
    let parsed = reqwest::Url::parse(base_url)
        .map_err(|error| ClientError::Http(format!("runtime endpoint URL is invalid: {error}")))?;
    let host_is_loopback = parsed.host_str().is_some_and(loopback_host);
    let transport_is_safe =
        parsed.scheme() == "https" || (parsed.scheme() == "http" && host_is_loopback);
    let is_root_url = parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.path() == "/"
        && parsed.query().is_none()
        && parsed.fragment().is_none();
    if transport_is_safe && is_root_url {
        return Ok(());
    }
    Err(ClientError::Http(
        "remote app-server endpoint must use a root HTTPS URL or loopback HTTP URL".to_owned(),
    ))
}

fn loopback_host(host: &str) -> bool {
    let address_host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    address_host.eq_ignore_ascii_case("localhost")
        || address_host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

pub(crate) fn validate_local_app_server_base_url(base_url: &str) -> Result<(), ClientError> {
    let parsed = reqwest::Url::parse(base_url).map_err(|error| {
        ClientError::Daemon(format!("runtime endpoint base URL is invalid: {error}"))
    })?;
    let host_is_loopback = parsed.host_str().is_some_and(loopback_host);
    let is_root_http_url = parsed.scheme() == "http"
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.path() == "/"
        && parsed.query().is_none()
        && parsed.fragment().is_none();
    if host_is_loopback && is_root_http_url {
        return Ok(());
    }
    Err(ClientError::Daemon(
        "local app-server endpoint must use a root HTTP URL on a loopback address".to_owned(),
    ))
}

#[async_trait]
impl RuntimeClient for HttpSseTransport {
    async fn send_command(&self, command: SessionCommand) -> Result<CommandAck, ClientError> {
        let command = encode_command_value_for_protocol(&command, self.protocol_version).map_err(
            |error| ClientError::Daemon(format!("command wire encoding failed: {error}")),
        )?;
        let response = self
            .send_attached(|attachment_id| {
                self.authenticated(self.client.post(self.url("/commands")))
                    .header(APP_SERVER_ATTACHMENT_HEADER, attachment_id)
                    .json(&command)
                    .timeout(Duration::from_secs(30))
            })
            .await?;
        decode_http_response(response).await
    }

    async fn query(&self, query: RuntimeQuery) -> Result<Value, ClientError> {
        let response = self
            .send_attached(|attachment_id| {
                self.authenticated(self.client.post(self.url("/queries")))
                    .header(APP_SERVER_ATTACHMENT_HEADER, attachment_id)
                    .json(&query)
                    .timeout(Duration::from_secs(30))
            })
            .await?;
        decode_http_response(response).await
    }

    async fn event_page(&self, request: EventPageRequest) -> Result<EventPage, ClientError> {
        let response = self
            .send_attached(|attachment_id| {
                self.authenticated(self.client.get(self.url("/events/page")))
                    .header(APP_SERVER_ATTACHMENT_HEADER, attachment_id)
                    .query(&request)
                    .timeout(Duration::from_secs(30))
            })
            .await?;
        decode_http_response(response).await
    }

    async fn replay_events(&self, filter: EventFilter) -> Result<Vec<Value>, ClientError> {
        let response = self
            .send_attached(|attachment_id| {
                let mut request = self
                    .authenticated(self.client.get(self.url("/events/replay")))
                    .header(APP_SERVER_ATTACHMENT_HEADER, attachment_id)
                    .query(&[("session_id", filter.session_id.to_string())]);
                if let Some(task_id) = filter.task_id {
                    request = request.query(&[("task_id", task_id.to_string())]);
                }
                if let Some(cursor) = filter.after_sequence_no {
                    request = request.query(&[("cursor", cursor)]);
                }
                request.timeout(Duration::from_secs(30))
            })
            .await?;
        decode_http_response(response).await
    }

    async fn subscribe(&self, filter: EventFilter) -> Result<RuntimeEventStream, ClientError> {
        self.ensure_open()?;
        let (sender, receiver) = mpsc::channel(256);
        let transport = self.clone();
        tokio::spawn(async move {
            transport.run_sse_subscription(filter, sender).await;
        });
        Ok(RuntimeEventStream::new(receiver))
    }
}

#[async_trait]
impl TaskTraceClient for HttpSseTransport {
    async fn task_trace(&self, request: TaskTraceRequest) -> Result<TaskTracePage, ClientError> {
        let response = self
            .send_attached(|attachment_id| {
                self.authenticated(self.client.post(self.url("/traces")))
                    .header(APP_SERVER_ATTACHMENT_HEADER, attachment_id)
                    .json(&request)
                    .timeout(Duration::from_secs(30))
            })
            .await?;
        decode_http_response(response).await
    }

    async fn read_artifact_chunk(
        &self,
        request: ArtifactReadRequest,
    ) -> Result<Option<ArtifactChunk>, ClientError> {
        let response = self
            .send_attached(|attachment_id| {
                self.authenticated(self.client.post(self.url("/artifacts/chunk")))
                    .header(APP_SERVER_ATTACHMENT_HEADER, attachment_id)
                    .json(&request)
                    .timeout(Duration::from_secs(30))
            })
            .await?;
        decode_http_response(response).await
    }
}

impl HttpSseTransport {
    async fn run_sse_subscription(
        self,
        filter: EventFilter,
        sender: mpsc::Sender<Result<RuntimeEvent, ClientError>>,
    ) {
        let mut cursor = filter.after_sequence_no;
        let mut retry_delay = Duration::from_millis(100);
        let cancellation = self.lifecycle.cancellation();
        loop {
            if sender.is_closed() || self.lifecycle.is_closed() {
                return;
            }
            let previous_cursor = cursor;
            let result = self
                .consume_sse_connection(&filter, &mut cursor, &sender)
                .await;
            let made_progress = cursor != previous_cursor;
            if sender.is_closed() || self.lifecycle.is_closed() {
                return;
            }
            if let Err(error) = result {
                // A dropped SSE connection is a normal reconnect boundary.
                // Do not surface it through RuntimeEventStream: doing so
                // makes AgentClient fail before the background subscription
                // can replay the missing facts. Permanent HTTP/protocol
                // failures are still delivered to the consumer.
                if is_permanent_subscription_error(&error) {
                    let _ = sender.send(Err(error)).await;
                    return;
                }
            }
            if made_progress {
                retry_delay = Duration::from_millis(100);
            }
            tokio::select! {
                _ = cancellation.cancelled() => return,
                _ = tokio::time::sleep(retry_delay) => {}
            }
            if !made_progress {
                retry_delay = (retry_delay * 2).min(Duration::from_secs(2));
            }
        }
    }

    async fn consume_sse_connection(
        &self,
        filter: &EventFilter,
        cursor: &mut Option<u64>,
        sender: &mpsc::Sender<Result<RuntimeEvent, ClientError>>,
    ) -> Result<(), ClientError> {
        let cancellation = self.lifecycle.cancellation();
        let response = self
            .send_attached_for_subscription(&cancellation, |attachment_id| {
                let mut request = self
                    .authenticated(self.client.get(self.url("/events")))
                    .header(APP_SERVER_ATTACHMENT_HEADER, attachment_id)
                    .query(&[("session_id", filter.session_id.to_string())]);
                if let Some(task_id) = filter.task_id {
                    request = request.query(&[("task_id", task_id.to_string())]);
                }
                if let Some(sequence_no) = *cursor {
                    request = request
                        .query(&[("cursor", sequence_no)])
                        .header("last-event-id", sequence_no.to_string());
                }
                request
            })
            .await?;
        if !response.status().is_success() {
            let (status, body) = tokio::select! {
                _ = cancellation.cancelled() => return Ok(()),
                result = read_bounded_http_body(response) => result?,
            };
            return Err(http_status_error(
                status,
                &body,
                "SSE response body unavailable",
            ));
        }

        let mut chunks = response.bytes_stream();
        let mut frame = Vec::new();
        loop {
            let chunk = tokio::select! {
                _ = cancellation.cancelled() => return Ok(()),
                chunk = chunks.next() => chunk,
            };
            let Some(chunk) = chunk else {
                break;
            };
            let chunk = chunk.map_err(|error| ClientError::Http(error.to_string()))?;
            for byte in chunk {
                frame.push(byte);
                if frame.len() > MAX_SSE_EVENT_BYTES {
                    return Err(ClientError::Protocol(format!(
                        "SSE event exceeds {MAX_SSE_EVENT_BYTES} byte limit"
                    )));
                }
                if !sse_frame_complete(&frame) {
                    continue;
                }
                let event = parse_sse_frame(&frame)?;
                frame.clear();
                let Some(event) = event else {
                    continue;
                };
                let Some(runtime_event) = decode_sse_runtime_event(event)? else {
                    continue;
                };
                if cursor.is_some_and(|sequence_no| runtime_event.sequence_no <= sequence_no) {
                    continue;
                }
                let sequence_no = runtime_event.sequence_no;
                if sender.send(Ok(runtime_event)).await.is_err() {
                    return Ok(());
                }
                *cursor = Some(sequence_no);
            }
        }
        Err(ClientError::Http("SSE connection closed".to_owned()))
    }
}

fn decode_sse_runtime_event(event: ParsedSseEvent) -> Result<Option<RuntimeEvent>, ClientError> {
    if event.event == "lag" {
        return Ok(None);
    }
    if event.event == "error" {
        return Err(ClientError::Protocol(format!(
            "runtime SSE error event: {}",
            event.data
        )));
    }
    decode_event(event.data.as_bytes())
        .map(Some)
        .map_err(|error| {
            ClientError::Protocol(format!("runtime event wire decoding failed: {error}"))
        })
}

fn is_permanent_subscription_error(error: &ClientError) -> bool {
    match error {
        ClientError::Protocol(_)
        | ClientError::Serialization(_)
        | ClientError::InvalidSession(_) => true,
        ClientError::Http(message) | ClientError::Daemon(message) => subscription_status(message)
            .is_some_and(|status| {
                (400..500).contains(&status) && !matches!(status, 408 | 410 | 429)
            }),
        _ => false,
    }
}

fn subscription_status(message: &str) -> Option<u16> {
    let value = message
        .strip_prefix("HTTP ")
        .or_else(|| message.strip_prefix("IPC HTTP "))?
        .trim_start();
    let digits = value
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect::<String>();
    (!digits.is_empty())
        .then(|| digits.parse::<u16>().ok())
        .flatten()
}

#[cfg(test)]
mod lifecycle_tests {
    use std::{
        path::PathBuf,
        sync::{Arc, RwLock},
    };

    use super::{
        AppServerInfo, HttpSseTransport, RuntimeClient, RuntimeHostInfo, TransportLifecycle,
        is_permanent_subscription_error,
    };
    use crate::ClientError;
    use golutra_core::{SessionId, ThreadId, WorkspaceId};
    use golutra_protocol::{EventFilter, ProtocolVersionRange};
    use secrecy::SecretString;

    fn test_server_info() -> AppServerInfo {
        AppServerInfo {
            instance_id: "server".to_owned(),
            pid: 1,
            base_url: "http://127.0.0.1:9".to_owned(),
            ipc_path: None,
            protocol_versions: ProtocolVersionRange::runtime(),
            started_at: chrono::Utc::now(),
        }
    }

    fn test_runtime_info(cwd: &str) -> RuntimeHostInfo {
        RuntimeHostInfo {
            instance_id: "runtime".to_owned(),
            pid: 1,
            base_url: "http://127.0.0.1:9".to_owned(),
            cwd: cwd.to_owned(),
            workspace_id: WorkspaceId::new(),
            default_session_id: SessionId::new(),
            default_thread_id: ThreadId::new(),
            started_at: chrono::Utc::now(),
        }
    }

    fn test_filter() -> EventFilter {
        EventFilter {
            session_id: SessionId::new(),
            task_id: None,
            after_sequence_no: None,
        }
    }

    fn test_http_transport() -> HttpSseTransport {
        HttpSseTransport {
            client: reqwest::Client::new(),
            base_url: "http://127.0.0.1:9".to_owned(),
            server_info: test_server_info(),
            protocol_version: crate::RUNTIME_PROTOCOL_VERSION,
            info: test_runtime_info("/workspace"),
            cwd: PathBuf::from("/workspace"),
            attachment_id: Arc::new(RwLock::new("attachment".to_owned())),
            refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
            lifecycle: TransportLifecycle::default(),
            transport_token: Arc::new(SecretString::from("a".repeat(64))),
        }
    }

    #[test]
    fn closing_a_transport_cancels_all_cloned_subscription_lifecycles() {
        let lifecycle = TransportLifecycle::default();
        let clone = lifecycle.clone();
        let cancellation = clone.cancellation();

        lifecycle.close();

        assert!(clone.is_closed());
        assert!(cancellation.is_cancelled());
    }

    #[test]
    fn subscription_retries_transient_transport_failures_but_surfaces_client_errors() {
        assert!(!is_permanent_subscription_error(&ClientError::Http(
            "SSE connection closed".to_owned(),
        )));
        assert!(!is_permanent_subscription_error(&ClientError::Daemon(
            "connection reset by peer".to_owned(),
        )));
        assert!(is_permanent_subscription_error(&ClientError::Http(
            "HTTP 400: invalid session".to_owned(),
        )));
        assert!(is_permanent_subscription_error(&ClientError::Daemon(
            "IPC HTTP 404: runtime attachment was not found".to_owned(),
        )));
        assert!(!is_permanent_subscription_error(&ClientError::Http(
            "HTTP 408: request timed out".to_owned(),
        )));
        assert!(!is_permanent_subscription_error(&ClientError::Daemon(
            "IPC HTTP 410: runtime attachment expired".to_owned(),
        )));
        assert!(!is_permanent_subscription_error(&ClientError::Daemon(
            "IPC HTTP 429: rate limited".to_owned(),
        )));
        assert!(is_permanent_subscription_error(&ClientError::Http(
            "HTTP 401: unauthorized".to_owned(),
        )));
        assert!(is_permanent_subscription_error(&ClientError::Http(
            "HTTP 403: forbidden".to_owned(),
        )));
    }

    #[test]
    fn http_error_messages_are_short_and_preserve_utf8_boundaries() {
        let message = "错误".repeat(super::MAX_HTTP_ERROR_MESSAGE_BYTES);
        let truncated = super::truncate_http_error_message(&message);

        assert!(truncated.ends_with("..."));
        assert!(truncated.len() <= super::MAX_HTTP_ERROR_MESSAGE_BYTES + 3);
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
    }

    #[test]
    fn malformed_sse_events_are_permanent_protocol_failures() {
        let named_error = super::decode_sse_runtime_event(super::ParsedSseEvent {
            event: "error".to_owned(),
            data: "{\"error\":\"bad event\"}".to_owned(),
        })
        .expect_err("named SSE errors must terminate the stream");
        assert!(matches!(named_error, ClientError::Protocol(_)));
        assert!(is_permanent_subscription_error(&named_error));

        let malformed_wire = super::decode_sse_runtime_event(super::ParsedSseEvent {
            event: "message".to_owned(),
            data: "not-json".to_owned(),
        })
        .expect_err("malformed event wire must terminate the stream");
        assert!(matches!(malformed_wire, ClientError::Protocol(_)));
        assert!(is_permanent_subscription_error(&malformed_wire));
    }

    #[tokio::test]
    async fn http_subscription_rejects_a_closed_transport_before_spawning() {
        let transport = test_http_transport();
        transport.lifecycle.close();

        let error = transport
            .subscribe(test_filter())
            .await
            .expect_err("closed HTTP transport must reject subscriptions");
        assert!(
            matches!(error, ClientError::Http(message) if message == "runtime transport is closed")
        );
    }

    #[tokio::test]
    async fn failed_http_close_keeps_the_attachment_id_for_a_retry() {
        let transport = test_http_transport();

        assert!(transport.close().await.is_err());
        assert!(transport.lifecycle.is_closed());
        assert_eq!(
            transport.current_attachment_id().expect("attachment id"),
            "attachment"
        );

        assert!(transport.close().await.is_err());
        assert_eq!(
            transport.current_attachment_id().expect("attachment id"),
            "attachment"
        );
    }

    #[tokio::test]
    async fn closing_http_transport_interrupts_a_pending_response_head() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("listener address");
        let (accepted_sender, accepted_receiver) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("connection");
            let _ = accepted_sender.send(());
            let _stream = stream;
            std::future::pending::<()>().await;
        });

        let mut transport = test_http_transport();
        transport.base_url = format!("http://{address}");
        let cancellation = transport.lifecycle.cancellation();
        let request = transport.send_attached_for_subscription(&cancellation, |_| {
            transport.client.get(transport.url("/events"))
        });
        tokio::pin!(request);
        tokio::select! {
            result = &mut request => panic!("response head unexpectedly completed: {result:?}"),
            _ = accepted_receiver => {}
        }

        transport.lifecycle.close();
        let error = tokio::time::timeout(std::time::Duration::from_secs(1), &mut request)
            .await
            .expect("transport close must interrupt the request")
            .expect_err("closed transport should return an error");
        assert!(matches!(
            error,
            ClientError::Http(message) if message == "runtime transport is closed"
        ));

        server.abort();
        let _ = server.await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ipc_subscription_rejects_a_closed_transport_before_spawning() {
        let transport = super::UnixIpcTransport {
            socket_path: PathBuf::from("/tmp/golutra-test.sock"),
            server_info: test_server_info(),
            protocol_version: crate::RUNTIME_PROTOCOL_VERSION,
            info: test_runtime_info("/workspace"),
            cwd: PathBuf::from("/workspace"),
            attachment_id: Arc::new(RwLock::new("attachment".to_owned())),
            refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
            lifecycle: TransportLifecycle::default(),
            transport_token: Arc::new(SecretString::from("a".repeat(64))),
        };
        transport.lifecycle.close();

        let error = transport
            .subscribe(test_filter())
            .await
            .expect_err("closed IPC transport must reject subscriptions");
        assert!(
            matches!(error, ClientError::Daemon(message) if message == "runtime transport is closed")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_ipc_close_keeps_the_attachment_id_for_a_retry() {
        let transport = super::UnixIpcTransport {
            socket_path: PathBuf::from("/tmp/golutra-missing-close.sock"),
            server_info: test_server_info(),
            protocol_version: crate::RUNTIME_PROTOCOL_VERSION,
            info: test_runtime_info("/workspace"),
            cwd: PathBuf::from("/workspace"),
            attachment_id: Arc::new(RwLock::new("attachment".to_owned())),
            refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
            lifecycle: TransportLifecycle::default(),
            transport_token: Arc::new(SecretString::from("a".repeat(64))),
        };

        assert!(transport.close().await.is_err());
        assert!(transport.lifecycle.is_closed());
        assert_eq!(
            transport.current_attachment_id().expect("attachment id"),
            "attachment"
        );

        assert!(transport.close().await.is_err());
        assert_eq!(
            transport.current_attachment_id().expect("attachment id"),
            "attachment"
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedSseEvent {
    pub(crate) event: String,
    pub(crate) data: String,
}

pub(crate) fn sse_frame_complete(frame: &[u8]) -> bool {
    frame.ends_with(b"\n\n") || frame.ends_with(b"\r\n\r\n")
}

pub(crate) fn parse_sse_frame(frame: &[u8]) -> Result<Option<ParsedSseEvent>, ClientError> {
    let frame = std::str::from_utf8(frame)
        .map_err(|error| ClientError::Protocol(format!("SSE event is not valid UTF-8: {error}")))?;
    let mut event = "message".to_owned();
    let mut data = Vec::new();
    for line in frame.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = line.split_once(':').map_or((line, ""), |(field, value)| {
            (field, value.strip_prefix(' ').unwrap_or(value))
        });
        match field {
            "event" => event = value.to_owned(),
            "data" => data.push(value),
            _ => {}
        }
    }
    if data.is_empty() {
        Ok(None)
    } else {
        Ok(Some(ParsedSseEvent {
            event,
            data: data.join("\n"),
        }))
    }
}

#[derive(Debug, Clone)]
pub enum RuntimeTransport {
    Embedded(EmbeddedTransport),
    LocalDaemon(HttpSseTransport),
    #[cfg(unix)]
    LocalIpc(UnixIpcTransport),
    Remote(HttpSseTransport),
}

impl RuntimeTransport {
    pub async fn in_memory() -> Result<Self, ClientError> {
        EmbeddedTransport::in_memory().await.map(Self::Embedded)
    }

    pub async fn ephemeral_for_cwd(cwd: impl AsRef<Path>) -> Result<Self, ClientError> {
        EmbeddedTransport::ephemeral_for_cwd(cwd)
            .await
            .map(Self::Embedded)
    }

    pub async fn ephemeral_for_cwd_with_options(
        cwd: impl AsRef<Path>,
        execution_options: RuntimeExecutionOptions,
    ) -> Result<Self, ClientError> {
        EmbeddedTransport::ephemeral_for_cwd_with_options(cwd, execution_options)
            .await
            .map(Self::Embedded)
    }

    pub async fn ephemeral_persistent_for_cwd(
        cwd: impl AsRef<Path>,
        state_home: impl AsRef<Path>,
    ) -> Result<Self, ClientError> {
        EmbeddedTransport::ephemeral_persistent_for_cwd(cwd, state_home)
            .await
            .map(Self::Embedded)
    }

    pub async fn ephemeral_persistent_for_cwd_with_options(
        cwd: impl AsRef<Path>,
        state_home: impl AsRef<Path>,
        execution_options: RuntimeExecutionOptions,
    ) -> Result<Self, ClientError> {
        EmbeddedTransport::ephemeral_persistent_for_cwd_with_options(
            cwd,
            state_home,
            execution_options,
        )
        .await
        .map(Self::Embedded)
    }

    pub async fn open_persisted_run(run_root: impl AsRef<Path>) -> Result<Self, ClientError> {
        EmbeddedTransport::open_persisted_run(run_root)
            .await
            .map(Self::Embedded)
    }

    pub async fn for_current_cwd() -> Result<Self, ClientError> {
        let cwd = std::env::current_dir().map_err(|error| ClientError::Io(error.to_string()))?;
        Self::for_cwd(cwd).await
    }

    pub async fn for_cwd(cwd: impl AsRef<Path>) -> Result<Self, ClientError> {
        EmbeddedTransport::for_cwd(cwd).await.map(Self::Embedded)
    }

    pub async fn for_cwd_with_options(
        cwd: impl AsRef<Path>,
        execution_options: RuntimeExecutionOptions,
    ) -> Result<Self, ClientError> {
        EmbeddedTransport::for_cwd_with_options(cwd, execution_options)
            .await
            .map(Self::Embedded)
    }

    pub async fn local_daemon(cwd: impl AsRef<Path>) -> Result<Self, ClientError> {
        #[cfg(unix)]
        {
            UnixIpcTransport::connect_local_daemon(cwd)
                .await
                .map(Self::LocalIpc)
        }
        #[cfg(not(unix))]
        {
            HttpSseTransport::connect_local_daemon(cwd)
                .await
                .map(Self::LocalDaemon)
        }
    }

    pub async fn connect(
        base_url: impl Into<String>,
        cwd: impl AsRef<Path>,
    ) -> Result<Self, ClientError> {
        HttpSseTransport::connect(base_url, cwd)
            .await
            .map(Self::Remote)
    }

    pub async fn connect_with_token(
        base_url: impl Into<String>,
        cwd: impl AsRef<Path>,
        transport_token: SecretString,
    ) -> Result<Self, ClientError> {
        HttpSseTransport::connect_with_token(base_url, cwd, transport_token)
            .await
            .map(Self::Remote)
    }

    /// Release the transport's runtime ownership. Embedded runtimes await
    /// managed-process shutdown; network transports revoke their attachment.
    pub async fn close(&self) -> Result<(), ClientError> {
        match self {
            Self::Embedded(transport) => transport.close().await,
            Self::LocalDaemon(transport) | Self::Remote(transport) => transport.close().await,
            #[cfg(unix)]
            Self::LocalIpc(transport) => transport.close().await,
        }
    }

    /// Whether this client renders a runtime owned by another process.
    ///
    /// Remote clients may query and control the runtime, but provider files,
    /// OAuth callbacks and other host-local side effects belong to the app
    /// server process rather than the client process.
    #[must_use]
    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Remote(_))
    }

    /// Return the actor identity that the runtime uses for control commands.
    /// Embedded runtimes preserve the frontend actor; app-server attachments
    /// use a server-owned actor so callers cannot spoof control ownership.
    #[must_use]
    pub fn control_actor_id(&self, embedded_actor_id: &str) -> String {
        match self {
            Self::Embedded(_) => embedded_actor_id.to_owned(),
            #[cfg(unix)]
            Self::LocalIpc(transport) => transport
                .current_attachment_actor_id()
                .unwrap_or_else(|_| embedded_actor_id.to_owned()),
            Self::LocalDaemon(transport) | Self::Remote(transport) => transport
                .current_attachment_actor_id()
                .unwrap_or_else(|_| embedded_actor_id.to_owned()),
        }
    }

    #[must_use]
    pub fn default_session_id(&self) -> SessionId {
        match self {
            Self::Embedded(transport) => transport.default_session_id(),
            #[cfg(unix)]
            Self::LocalIpc(transport) => transport.info.default_session_id,
            Self::LocalDaemon(transport) | Self::Remote(transport) => {
                transport.info.default_session_id
            }
        }
    }

    #[must_use]
    pub fn default_thread_id(&self) -> ThreadId {
        match self {
            Self::Embedded(transport) => transport.default_thread_id(),
            #[cfg(unix)]
            Self::LocalIpc(transport) => transport.info.default_thread_id,
            Self::LocalDaemon(transport) | Self::Remote(transport) => {
                transport.info.default_thread_id
            }
        }
    }

    #[must_use]
    pub fn cwd(&self) -> Option<&Path> {
        match self {
            Self::Embedded(transport) => transport.cwd(),
            #[cfg(unix)]
            Self::LocalIpc(transport) => Some(&transport.cwd),
            Self::LocalDaemon(transport) | Self::Remote(transport) => Some(&transport.cwd),
        }
    }

    #[must_use]
    pub fn workspace_id(&self) -> WorkspaceId {
        match self {
            Self::Embedded(transport) => transport.workspace_id(),
            #[cfg(unix)]
            Self::LocalIpc(transport) => transport.info.workspace_id,
            Self::LocalDaemon(transport) | Self::Remote(transport) => transport.info.workspace_id,
        }
    }

    pub async fn list_threads(&self, limit: u32) -> Result<Vec<ThreadRecord>, ClientError> {
        match self {
            Self::Embedded(transport) => transport.list_threads(limit).await,
            #[cfg(unix)]
            Self::LocalIpc(transport) => transport.list_threads(limit).await,
            Self::LocalDaemon(transport) | Self::Remote(transport) => {
                transport.list_threads(limit).await
            }
        }
    }

    pub async fn session_page(
        &self,
        request: SessionPageRequest,
    ) -> Result<SessionPage, ClientError> {
        match self {
            Self::Embedded(transport) => transport.session_page(request).await,
            #[cfg(unix)]
            Self::LocalIpc(transport) => transport.session_page(request).await,
            Self::LocalDaemon(transport) | Self::Remote(transport) => {
                transport.session_page(request).await
            }
        }
    }

    pub async fn session_window(
        &self,
        request: SessionWindowRequest,
    ) -> Result<SessionWindow, ClientError> {
        match self {
            Self::Embedded(transport) => transport.session_window(request).await,
            #[cfg(unix)]
            Self::LocalIpc(transport) => transport.session_window(request).await,
            Self::LocalDaemon(transport) | Self::Remote(transport) => {
                transport.session_window(request).await
            }
        }
    }

    pub async fn thread_for_session(
        &self,
        session_id: SessionId,
    ) -> Result<Option<ThreadRecord>, ClientError> {
        match self {
            Self::Embedded(transport) => transport.thread_for_session(session_id).await,
            #[cfg(unix)]
            Self::LocalIpc(transport) => transport.thread_for_session(session_id).await,
            Self::LocalDaemon(transport) | Self::Remote(transport) => {
                transport.thread_for_session(session_id).await
            }
        }
    }

    pub async fn resume_thread(&self, thread_id: ThreadId) -> Result<ThreadRecord, ClientError> {
        match self {
            Self::Embedded(transport) => transport.resume_thread(thread_id).await,
            #[cfg(unix)]
            Self::LocalIpc(transport) => transport.resume_thread(thread_id).await,
            Self::LocalDaemon(transport) | Self::Remote(transport) => {
                transport.resume_thread(thread_id).await
            }
        }
    }

    pub async fn fork_thread(
        &self,
        thread_id: ThreadId,
        from_turn_id: Option<TurnId>,
    ) -> Result<ThreadRecord, ClientError> {
        match self {
            Self::Embedded(transport) => transport.fork_thread(thread_id, from_turn_id).await,
            #[cfg(unix)]
            Self::LocalIpc(transport) => transport.fork_thread(thread_id, from_turn_id).await,
            Self::LocalDaemon(transport) | Self::Remote(transport) => {
                transport.fork_thread(thread_id, from_turn_id).await
            }
        }
    }

    pub async fn export_thread_rollout(
        &self,
        thread_id: ThreadId,
    ) -> Result<RolloutExport, ClientError> {
        match self {
            Self::Embedded(transport) => transport.export_thread_rollout(thread_id).await,
            #[cfg(unix)]
            Self::LocalIpc(transport) => transport.export_thread_rollout(thread_id).await,
            Self::LocalDaemon(transport) | Self::Remote(transport) => {
                transport.export_thread_rollout(thread_id).await
            }
        }
    }

    pub async fn rebind_thread(
        &self,
        thread_id: ThreadId,
        from_workspace_root: impl AsRef<Path>,
    ) -> Result<ThreadRebindResult, ClientError> {
        match self {
            Self::Embedded(transport) => {
                transport
                    .rebind_thread(thread_id, from_workspace_root)
                    .await
            }
            #[cfg(unix)]
            Self::LocalIpc(transport) => {
                transport
                    .rebind_thread(thread_id, from_workspace_root)
                    .await
            }
            Self::LocalDaemon(transport) | Self::Remote(transport) => {
                transport
                    .rebind_thread(thread_id, from_workspace_root)
                    .await
            }
        }
    }
}

#[async_trait]
impl RuntimeClient for RuntimeTransport {
    async fn send_command(&self, command: SessionCommand) -> Result<CommandAck, ClientError> {
        match self {
            Self::Embedded(transport) => transport.send_command(command).await,
            #[cfg(unix)]
            Self::LocalIpc(transport) => transport.send_command(command).await,
            Self::LocalDaemon(transport) | Self::Remote(transport) => {
                transport.send_command(command).await
            }
        }
    }

    async fn query(&self, query: RuntimeQuery) -> Result<Value, ClientError> {
        match self {
            Self::Embedded(transport) => transport.query(query).await,
            #[cfg(unix)]
            Self::LocalIpc(transport) => transport.query(query).await,
            Self::LocalDaemon(transport) | Self::Remote(transport) => transport.query(query).await,
        }
    }

    async fn event_page(&self, request: EventPageRequest) -> Result<EventPage, ClientError> {
        match self {
            Self::Embedded(transport) => transport.event_page(request).await,
            #[cfg(unix)]
            Self::LocalIpc(transport) => transport.event_page(request).await,
            Self::LocalDaemon(transport) | Self::Remote(transport) => {
                transport.event_page(request).await
            }
        }
    }

    async fn replay_events(&self, filter: EventFilter) -> Result<Vec<Value>, ClientError> {
        match self {
            Self::Embedded(transport) => transport.replay_events(filter).await,
            #[cfg(unix)]
            Self::LocalIpc(transport) => transport.replay_events(filter).await,
            Self::LocalDaemon(transport) | Self::Remote(transport) => {
                transport.replay_events(filter).await
            }
        }
    }

    async fn subscribe(&self, filter: EventFilter) -> Result<RuntimeEventStream, ClientError> {
        match self {
            Self::Embedded(transport) => transport.subscribe(filter).await,
            #[cfg(unix)]
            Self::LocalIpc(transport) => transport.subscribe(filter).await,
            Self::LocalDaemon(transport) | Self::Remote(transport) => {
                transport.subscribe(filter).await
            }
        }
    }
}

#[async_trait]
impl TaskTraceClient for RuntimeTransport {
    async fn task_trace(&self, request: TaskTraceRequest) -> Result<TaskTracePage, ClientError> {
        match self {
            Self::Embedded(transport) => transport.task_trace(request).await,
            #[cfg(unix)]
            Self::LocalIpc(transport) => transport.task_trace(request).await,
            Self::LocalDaemon(transport) | Self::Remote(transport) => {
                transport.task_trace(request).await
            }
        }
    }

    async fn read_artifact_chunk(
        &self,
        request: ArtifactReadRequest,
    ) -> Result<Option<ArtifactChunk>, ClientError> {
        match self {
            Self::Embedded(transport) => transport.read_artifact_chunk(request).await,
            #[cfg(unix)]
            Self::LocalIpc(transport) => transport.read_artifact_chunk(request).await,
            Self::LocalDaemon(transport) | Self::Remote(transport) => {
                transport.read_artifact_chunk(request).await
            }
        }
    }
}

async fn decode_http_response<T>(response: reqwest::Response) -> Result<T, ClientError>
where
    T: DeserializeOwned,
{
    let (status, bytes) = read_bounded_http_body(response).await?;
    if !status.is_success() {
        return Err(http_status_error(status, &bytes, "HTTP request failed"));
    }
    serde_json::from_slice(&bytes).map_err(ClientError::Serialization)
}

async fn read_bounded_http_body(
    response: reqwest::Response,
) -> Result<(reqwest::StatusCode, Vec<u8>), ClientError> {
    let status = response.status();
    if response
        .content_length()
        .is_some_and(|length| length > MAX_HTTP_JSON_RESPONSE_BYTES as u64)
    {
        return Err(ClientError::Http(format!(
            "HTTP {status} response exceeds {MAX_HTTP_JSON_RESPONSE_BYTES} byte limit"
        )));
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| ClientError::Http(error.to_string()))?;
        if bytes.len().saturating_add(chunk.len()) > MAX_HTTP_JSON_RESPONSE_BYTES {
            return Err(ClientError::Http(format!(
                "HTTP {status} response exceeds {MAX_HTTP_JSON_RESPONSE_BYTES} byte limit"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((status, bytes))
}

fn http_status_error(status: reqwest::StatusCode, body: &[u8], fallback: &str) -> ClientError {
    let message = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| {
            if body.is_empty() {
                fallback.to_owned()
            } else {
                String::from_utf8_lossy(body).into_owned()
            }
        });
    ClientError::Http(format!(
        "HTTP {status}: {}",
        truncate_http_error_message(&message)
    ))
}

fn truncate_http_error_message(message: &str) -> String {
    if message.len() <= MAX_HTTP_ERROR_MESSAGE_BYTES {
        return message.to_owned();
    }
    let mut end = MAX_HTTP_ERROR_MESSAGE_BYTES;
    while !message.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}...", &message[..end])
}

pub(crate) async fn run_blocking<T, F>(operation: F) -> Result<T, ClientError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| ClientError::TaskExecution(error.to_string()))
}
