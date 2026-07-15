//! Runtime transport 实现与 HTTP/SSE 协议边界。

use async_trait::async_trait;
use futures_util::StreamExt;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::sync::RwLock;

use super::*;

#[cfg(unix)]
mod ipc;
#[cfg(unix)]
pub use ipc::UnixIpcTransport;

#[async_trait]
pub trait RuntimeClient {
    async fn send_command(&self, command: SessionCommand) -> Result<CommandAck, ClientError>;
    async fn query(&self, query: RuntimeQuery) -> Result<Value, ClientError>;
    async fn event_page(&self, request: EventPageRequest) -> Result<EventPage, ClientError>;
    async fn replay_events(&self, filter: EventFilter) -> Result<Vec<Value>, ClientError>;
    async fn subscribe(&self, filter: EventFilter) -> Result<RuntimeEventStream, ClientError>;
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
    pub(crate) host: Arc<RuntimeHost>,
}

impl EmbeddedTransport {
    #[must_use]
    pub fn new(host: Arc<RuntimeHost>) -> Self {
        Self { host }
    }

    pub async fn in_memory() -> Result<Self, ClientError> {
        Ok(Self::new(RuntimeHost::in_memory().await?))
    }

    pub async fn for_current_cwd() -> Result<Self, ClientError> {
        let cwd = std::env::current_dir().map_err(|error| ClientError::Io(error.to_string()))?;
        Self::for_cwd(cwd).await
    }

    pub async fn for_cwd(cwd: impl AsRef<Path>) -> Result<Self, ClientError> {
        Ok(Self::new(RuntimeHost::for_cwd(cwd).await?))
    }

    pub async fn from_home_and_cwd(
        home: impl AsRef<Path>,
        cwd: impl AsRef<Path>,
    ) -> Result<Self, ClientError> {
        Ok(Self::new(RuntimeHost::from_home_and_cwd(home, cwd).await?))
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
    pub fn cwd(&self) -> Option<&Path> {
        self.host.workspace_root()
    }

    #[must_use]
    pub fn workspace_id(&self) -> WorkspaceId {
        self.host.workspace_id
    }

    #[must_use]
    pub fn subscribe_live(&self, filter: EventFilter) -> broadcast::Receiver<RuntimeEvent> {
        self.host.subscribe_live(filter)
    }

    pub async fn list_threads(&self, limit: u32) -> Result<Vec<ThreadRecord>, ClientError> {
        self.host.list_threads(limit).await
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
    ) -> Result<RolloutExport, ClientError> {
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

#[async_trait]
impl RuntimeClient for EmbeddedTransport {
    async fn send_command(&self, command: SessionCommand) -> Result<CommandAck, ClientError> {
        self.host.clone().handle_command(command).await
    }

    async fn query(&self, query: RuntimeQuery) -> Result<Value, ClientError> {
        self.host.query(query).await
    }

    async fn event_page(&self, request: EventPageRequest) -> Result<EventPage, ClientError> {
        self.host.event_page(request).await
    }

    async fn replay_events(&self, filter: EventFilter) -> Result<Vec<Value>, ClientError> {
        self.host.replay_events(filter).await
    }

    async fn subscribe(&self, filter: EventFilter) -> Result<RuntimeEventStream, ClientError> {
        self.host.clone().event_stream(filter).await
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
    pub(crate) info: RuntimeHostInfo,
    pub(crate) cwd: PathBuf,
    pub(crate) attachment_id: Arc<RwLock<String>>,
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
        )
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|error| ClientError::Http(error.to_string()))?;
        let server_info: AppServerInfo = decode_http_response(response).await?;
        if !server_info
            .protocol_versions
            .accepts(RUNTIME_PROTOCOL_VERSION)
        {
            return Err(ClientError::Http(format!(
                "runtime protocol {} is incompatible with server range {}..={}",
                RUNTIME_PROTOCOL_VERSION,
                server_info.protocol_versions.minimum,
                server_info.protocol_versions.current,
            )));
        }
        let response = authenticated_request(
            client.post(format!("{base_url}/runtime/attach")),
            &transport_token,
        )
        .json(&json!({
            "cwd": requested_cwd,
            "protocol_version": RUNTIME_PROTOCOL_VERSION,
        }))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|error| ClientError::Http(error.to_string()))?;
        let attachment: RuntimeAttachment = decode_http_response(response).await?;
        let attached_cwd = PathBuf::from(&attachment.runtime.cwd);
        if !attached_cwd.is_absolute() {
            return Err(ClientError::Http(format!(
                "runtime returned a non-absolute cwd: {}",
                attached_cwd.display()
            )));
        }
        Ok(Self {
            client,
            base_url,
            server_info,
            info: attachment.runtime,
            cwd: attached_cwd,
            attachment_id: Arc::new(RwLock::new(attachment.attachment_id)),
            transport_token: Arc::new(transport_token),
        })
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
            return Err(ClientError::Daemon(
                "app-server endpoint metadata does not match the running server".to_owned(),
            ));
        }
        if transport.cwd != paths.cwd {
            return Err(ClientError::Daemon(format!(
                "app-server attached `{}` instead of local cwd `{}`",
                transport.cwd.display(),
                paths.cwd.display()
            )));
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
        authenticated_request(request, &self.transport_token)
    }

    fn current_attachment_id(&self) -> Result<String, ClientError> {
        self.attachment_id
            .read()
            .map(|attachment_id| attachment_id.clone())
            .map_err(|_| ClientError::Http("runtime attachment lock is poisoned".to_owned()))
    }

    async fn refresh_attachment(&self, stale_attachment_id: &str) -> Result<String, ClientError> {
        let current = self.current_attachment_id()?;
        if current != stale_attachment_id {
            return Ok(current);
        }
        let response = self
            .authenticated(self.client.post(self.url("/runtime/attach")))
            .json(&json!({
                "cwd": self.cwd,
                "protocol_version": RUNTIME_PROTOCOL_VERSION,
            }))
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|error| ClientError::Http(error.to_string()))?;
        let attachment: RuntimeAttachment = decode_http_response(response).await?;
        let attached_cwd = PathBuf::from(&attachment.runtime.cwd);
        if attached_cwd != self.cwd {
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
        Ok(attachment_id)
    }

    async fn send_attached<F>(&self, build: F) -> Result<reqwest::Response, ClientError>
    where
        F: Fn(&str) -> reqwest::RequestBuilder,
    {
        let attachment_id = self.current_attachment_id()?;
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
}

fn authenticated_request(
    request: reqwest::RequestBuilder,
    transport_token: &SecretString,
) -> reqwest::RequestBuilder {
    request
        .bearer_auth(transport_token.expose_secret())
        .header(APP_SERVER_PROTOCOL_HEADER, RUNTIME_PROTOCOL_VERSION)
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
        let (sender, receiver) = mpsc::channel(256);
        let transport = self.clone();
        tokio::spawn(async move {
            transport.run_sse_subscription(filter, sender).await;
        });
        Ok(RuntimeEventStream::new(receiver))
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
        loop {
            if sender.is_closed() {
                return;
            }
            let previous_cursor = cursor;
            let result = self
                .consume_sse_connection(&filter, &mut cursor, &sender)
                .await;
            let made_progress = cursor != previous_cursor;
            if sender.is_closed() {
                return;
            }
            if let Err(error) = result
                && sender.send(Err(error)).await.is_err()
            {
                return;
            }
            if made_progress {
                retry_delay = Duration::from_millis(100);
            }
            tokio::time::sleep(retry_delay).await;
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
        let response = self
            .send_attached(|attachment_id| {
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
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "SSE response body unavailable".to_owned());
            return Err(ClientError::Http(format!("HTTP {status}: {body}")));
        }

        let mut chunks = response.bytes_stream();
        let mut frame = Vec::new();
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(|error| ClientError::Http(error.to_string()))?;
            for byte in chunk {
                frame.push(byte);
                if frame.len() > MAX_SSE_EVENT_BYTES {
                    return Err(ClientError::Http(format!(
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
                if event.event == "lag" {
                    continue;
                }
                if event.event == "error" {
                    return Err(ClientError::Http(event.data));
                }
                let runtime_event: RuntimeEvent = serde_json::from_str(&event.data)?;
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
        .map_err(|error| ClientError::Http(format!("SSE event is not valid UTF-8: {error}")))?;
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

    pub async fn for_current_cwd() -> Result<Self, ClientError> {
        let cwd = std::env::current_dir().map_err(|error| ClientError::Io(error.to_string()))?;
        Self::for_cwd(cwd).await
    }

    pub async fn for_cwd(cwd: impl AsRef<Path>) -> Result<Self, ClientError> {
        EmbeddedTransport::for_cwd(cwd).await.map(Self::Embedded)
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

async fn decode_http_response<T>(response: reqwest::Response) -> Result<T, ClientError>
where
    T: DeserializeOwned,
{
    let status = response.status();
    if response
        .content_length()
        .is_some_and(|length| length > MAX_HTTP_JSON_RESPONSE_BYTES as u64)
    {
        return Err(ClientError::Http(format!(
            "HTTP response exceeds {MAX_HTTP_JSON_RESPONSE_BYTES} byte limit"
        )));
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| ClientError::Http(error.to_string()))?;
        if bytes.len().saturating_add(chunk.len()) > MAX_HTTP_JSON_RESPONSE_BYTES {
            return Err(ClientError::Http(format!(
                "HTTP response exceeds {MAX_HTTP_JSON_RESPONSE_BYTES} byte limit"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        let message = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|value| {
                value
                    .get("error")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .unwrap_or_else(|| String::from_utf8_lossy(&bytes).to_string());
        return Err(ClientError::Http(format!("HTTP {status}: {message}")));
    }
    serde_json::from_slice(&bytes).map_err(ClientError::Serialization)
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
