use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use golutra_protocol::{
    IpcHttpRequest, IpcHttpResponseFrame, MAX_WIRE_MESSAGE_BYTES, encode_command_value_for_protocol,
};
use secrecy::{ExposeSecret, SecretString};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixStream, unix::OwnedReadHalf},
    sync::mpsc,
};

use super::*;

const MAX_IPC_FRAME_BYTES: usize = 128 * 1024;
// Match the bounded HTTP request lifetime. A command can legitimately spend time opening and
// migrating its workspace store before the router can produce a response head.
const IPC_RESPONSE_HEAD_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
struct IpcResponse {
    status: u16,
    body: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct UnixIpcTransport {
    pub(crate) socket_path: PathBuf,
    pub(crate) server_info: AppServerInfo,
    pub(crate) protocol_version: u32,
    pub(crate) info: RuntimeHostInfo,
    pub(crate) cwd: PathBuf,
    pub(crate) attachment_id: Arc<RwLock<String>>,
    pub(crate) refresh_lock: Arc<tokio::sync::Mutex<()>>,
    pub(crate) lifecycle: TransportLifecycle,
    pub(crate) transport_token: Arc<SecretString>,
}

impl UnixIpcTransport {
    pub async fn connect_local_daemon(cwd: impl AsRef<Path>) -> Result<Self, ClientError> {
        let paths = RuntimePaths::for_cwd(cwd.as_ref())?;
        Self::connect_from_paths(paths).await
    }

    pub async fn from_home_and_cwd(
        home: impl AsRef<Path>,
        cwd: impl AsRef<Path>,
    ) -> Result<Self, ClientError> {
        let paths = RuntimePaths::from_home_and_cwd(home, cwd)?;
        Self::connect_from_paths(paths).await
    }

    async fn connect_from_paths(paths: RuntimePaths) -> Result<Self, ClientError> {
        let endpoint_path = paths.app_server_endpoint.clone();
        let bytes = tokio::fs::read(&endpoint_path).await.map_err(|error| {
            ClientError::Daemon(format!("{}: {error}", endpoint_path.display()))
        })?;
        if bytes.len() > MAX_WIRE_MESSAGE_BYTES {
            return Err(ClientError::Daemon(
                "app-server endpoint metadata exceeds its size limit".to_owned(),
            ));
        }
        let endpoint: AppServerInfo = serde_json::from_slice(&bytes)?;
        validate_local_app_server_base_url(&endpoint.base_url)?;
        let endpoint_protocol_version = ProtocolVersionRange::runtime()
            .highest_common(endpoint.protocol_versions)
            .ok_or_else(|| {
                ClientError::Daemon(format!(
                    "runtime protocol ranges have no common version: client {}..={}, server {}..={}",
                    ProtocolVersionRange::runtime().minimum,
                    ProtocolVersionRange::runtime().current,
                    endpoint.protocol_versions.minimum,
                    endpoint.protocol_versions.current,
                ))
            })?;
        let advertised = endpoint.ipc_path.as_deref().ok_or_else(|| {
            ClientError::Daemon("app-server did not advertise a Unix IPC socket".to_owned())
        })?;
        let socket_path = PathBuf::from(advertised);
        if socket_path != paths.app_server_ipc_socket {
            return Err(ClientError::Daemon(format!(
                "app-server IPC path `{}` does not match expected path `{}`",
                socket_path.display(),
                paths.app_server_ipc_socket.display()
            )));
        }
        validate_socket(&socket_path).await?;
        let transport_token = read_transport_token(&paths.app_server_transport_token).await?;
        let server_info: AppServerInfo = decode_ipc_json(
            exchange(
                &socket_path,
                &transport_token,
                endpoint_protocol_version,
                None,
                "GET",
                "/runtime/info",
                None,
            )
            .await?,
        )?;
        if server_info.instance_id != endpoint.instance_id {
            return Err(ClientError::Daemon(
                "app-server IPC metadata does not match the running server".to_owned(),
            ));
        }
        let protocol_version = ProtocolVersionRange::runtime()
            .highest_common(server_info.protocol_versions)
            .ok_or_else(|| {
                ClientError::Daemon(format!(
                    "runtime protocol ranges have no common version: client {}..={}, server {}..={}",
                    ProtocolVersionRange::runtime().minimum,
                    ProtocolVersionRange::runtime().current,
                    server_info.protocol_versions.minimum,
                    server_info.protocol_versions.current,
                ))
            })?;
        let attachment: RuntimeAttachment = decode_ipc_json(
            exchange(
                &socket_path,
                &transport_token,
                protocol_version,
                None,
                "POST",
                "/runtime/attach",
                Some(json!({
                    "cwd": paths.cwd,
                    "protocol_version": protocol_version,
                })),
            )
            .await?,
        )?;
        let attached_cwd = PathBuf::from(&attachment.runtime.cwd);
        if attached_cwd != paths.cwd {
            let attachment_id = attachment.attachment_id;
            let _ = exchange(
                &socket_path,
                &transport_token,
                protocol_version,
                Some(&attachment_id),
                "DELETE",
                &format!("/runtime/attach/{attachment_id}"),
                None,
            )
            .await;
            return Err(ClientError::Daemon(format!(
                "app-server attached `{}` instead of local cwd `{}`",
                attached_cwd.display(),
                paths.cwd.display()
            )));
        }
        Ok(Self {
            socket_path,
            server_info,
            protocol_version,
            info: attachment.runtime,
            cwd: attached_cwd,
            attachment_id: Arc::new(RwLock::new(attachment.attachment_id)),
            refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
            lifecycle: TransportLifecycle::default(),
            transport_token: Arc::new(transport_token),
        })
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
        self.attached_json(
            "GET",
            &query_path("/threads", [("limit", limit.to_string())]),
            None,
        )
        .await
    }

    pub async fn session_page(
        &self,
        request: SessionPageRequest,
    ) -> Result<SessionPage, ClientError> {
        self.attached_json(
            "POST",
            "/sessions/page",
            Some(serde_json::to_value(request)?),
        )
        .await
    }

    pub async fn session_window(
        &self,
        request: SessionWindowRequest,
    ) -> Result<SessionWindow, ClientError> {
        self.attached_json(
            "POST",
            "/sessions/window",
            Some(serde_json::to_value(request)?),
        )
        .await
    }

    pub async fn thread_for_session(
        &self,
        session_id: SessionId,
    ) -> Result<Option<ThreadRecord>, ClientError> {
        self.attached_json("GET", &format!("/sessions/{session_id}/thread"), None)
            .await
    }

    pub async fn resume_thread(&self, thread_id: ThreadId) -> Result<ThreadRecord, ClientError> {
        self.attached_json("POST", &format!("/threads/{thread_id}/resume"), None)
            .await
    }

    pub async fn fork_thread(
        &self,
        thread_id: ThreadId,
        from_turn_id: Option<TurnId>,
    ) -> Result<ThreadRecord, ClientError> {
        self.attached_json(
            "POST",
            &format!("/threads/{thread_id}/fork"),
            Some(json!({"from_turn_id": from_turn_id})),
        )
        .await
    }

    pub async fn export_thread_rollout(
        &self,
        thread_id: ThreadId,
    ) -> Result<RolloutExport, ClientError> {
        self.attached_json(
            "POST",
            &format!("/threads/{thread_id}/rollout/export"),
            None,
        )
        .await
    }

    pub async fn rebind_thread(
        &self,
        thread_id: ThreadId,
        from_workspace_root: impl AsRef<Path>,
    ) -> Result<ThreadRebindResult, ClientError> {
        self.attached_json(
            "POST",
            &format!("/threads/{thread_id}/rebind"),
            Some(json!({
                "from_workspace_root": from_workspace_root.as_ref().display().to_string(),
            })),
        )
        .await
    }

    async fn attached_json<T: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<T, ClientError> {
        decode_ipc_json(self.attached_exchange(method, path, body).await?)
    }

    async fn attached_exchange(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<IpcResponse, ClientError> {
        self.ensure_open()?;
        let attachment_id = self.current_attachment_id()?;
        if attachment_id.is_empty() {
            return Err(ClientError::Daemon(
                "runtime transport has no attachment".to_owned(),
            ));
        }
        let response = exchange(
            &self.socket_path,
            &self.transport_token,
            self.protocol_version,
            Some(&attachment_id),
            method,
            path,
            body.clone(),
        )
        .await?;
        if response.status != 410 {
            return Ok(response);
        }
        let attachment_id = self.refresh_attachment(&attachment_id).await?;
        exchange(
            &self.socket_path,
            &self.transport_token,
            self.protocol_version,
            Some(&attachment_id),
            method,
            path,
            body,
        )
        .await
    }

    pub(crate) fn current_attachment_id(&self) -> Result<String, ClientError> {
        self.attachment_id
            .read()
            .map(|attachment_id| attachment_id.clone())
            .map_err(|_| ClientError::Daemon("runtime attachment lock is poisoned".to_owned()))
    }

    pub(crate) fn current_attachment_actor_id(&self) -> Result<String, ClientError> {
        self.ensure_open()?;
        self.current_attachment_id()
            .map(|id| app_server_attachment_actor_id(&id))
    }

    fn ensure_open(&self) -> Result<(), ClientError> {
        if self.lifecycle.is_closed() {
            return Err(ClientError::Daemon(
                "runtime transport is closed".to_owned(),
            ));
        }
        Ok(())
    }

    /// Explicitly release the server-side attachment capability. Local shutdown is idempotent,
    /// while a failed remote detach remains retryable until the server confirms that the
    /// capability is gone.
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
        let response = exchange(
            &self.socket_path,
            &self.transport_token,
            self.protocol_version,
            Some(&attachment_id),
            "DELETE",
            &format!("/runtime/attach/{attachment_id}"),
            None,
        )
        .await?;
        if (200..300).contains(&response.status) || matches!(response.status, 404 | 410) {
            *self.attachment_id.write().map_err(|_| {
                ClientError::Daemon("runtime attachment lock is poisoned".to_owned())
            })? = String::new();
            return Ok(());
        }
        Err(ipc_status_error(response.status, &response.body))
    }

    async fn refresh_attachment(&self, stale_attachment_id: &str) -> Result<String, ClientError> {
        let _refresh_guard = self.refresh_lock.lock().await;
        self.ensure_open()?;
        let current = self.current_attachment_id()?;
        if current.is_empty() {
            return Err(ClientError::Daemon(
                "runtime transport has no attachment".to_owned(),
            ));
        }
        if current != stale_attachment_id {
            return Ok(current);
        }
        let attachment: RuntimeAttachment = decode_ipc_json(
            exchange(
                &self.socket_path,
                &self.transport_token,
                self.protocol_version,
                None,
                "POST",
                "/runtime/attach",
                Some(json!({
                    "cwd": self.cwd,
                    "protocol_version": self.protocol_version,
                })),
            )
            .await?,
        )?;
        let attached_cwd = PathBuf::from(&attachment.runtime.cwd);
        if attached_cwd != self.cwd {
            let attachment_id = attachment.attachment_id;
            let _ = exchange(
                &self.socket_path,
                &self.transport_token,
                self.protocol_version,
                Some(&attachment_id),
                "DELETE",
                &format!("/runtime/attach/{attachment_id}"),
                None,
            )
            .await;
            return Err(ClientError::Daemon(format!(
                "runtime reattached `{}` instead of requested cwd `{}`",
                attached_cwd.display(),
                self.cwd.display()
            )));
        }
        let attachment_id = attachment.attachment_id;
        *self
            .attachment_id
            .write()
            .map_err(|_| ClientError::Daemon("runtime attachment lock is poisoned".to_owned()))? =
            attachment_id.clone();
        // Revoke the stale capability only after the replacement is active so concurrent
        // refreshes cannot accumulate unreachable control capabilities.
        let _ = exchange(
            &self.socket_path,
            &self.transport_token,
            self.protocol_version,
            Some(stale_attachment_id),
            "DELETE",
            &format!("/runtime/attach/{stale_attachment_id}"),
            None,
        )
        .await;
        Ok(attachment_id)
    }

    async fn run_subscription(
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
                .consume_subscription(&filter, &mut cursor, &sender)
                .await;
            let made_progress = cursor != previous_cursor;
            if sender.is_closed() || self.lifecycle.is_closed() {
                return;
            }
            if let Err(error) = result {
                // Socket resets and daemon restarts are reconnect boundaries, just like a
                // dropped HTTP SSE stream. Only deterministic protocol/session failures should
                // terminate the consumer.
                if super::is_permanent_subscription_error(&error) {
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

    async fn consume_subscription(
        &self,
        filter: &EventFilter,
        cursor: &mut Option<u64>,
        sender: &mpsc::Sender<Result<RuntimeEvent, ClientError>>,
    ) -> Result<(), ClientError> {
        let cancellation = self.lifecycle.cancellation();
        let path = event_path(filter, *cursor);
        self.ensure_open()?;
        let attachment_id = self.current_attachment_id()?;
        if attachment_id.is_empty() {
            return Err(ClientError::Daemon(
                "runtime transport has no attachment".to_owned(),
            ));
        }
        let (mut status, mut reader) = open_exchange_with_cancellation(
            IpcExchange {
                socket_path: &self.socket_path,
                token: &self.transport_token,
                protocol_version: self.protocol_version,
                attachment_id: Some(&attachment_id),
                method: "GET",
                path: &path,
                body: None,
            },
            Some(&cancellation),
        )
        .await?;
        if status == 410 {
            let attachment_id = tokio::select! {
                _ = cancellation.cancelled() => return Ok(()),
                result = self.refresh_attachment(&attachment_id) => result?,
            };
            (status, reader) = open_exchange_with_cancellation(
                IpcExchange {
                    socket_path: &self.socket_path,
                    token: &self.transport_token,
                    protocol_version: self.protocol_version,
                    attachment_id: Some(&attachment_id),
                    method: "GET",
                    path: &path,
                    body: None,
                },
                Some(&cancellation),
            )
            .await?;
        }
        if !(200..300).contains(&status) {
            let body = tokio::select! {
                _ = cancellation.cancelled() => return Ok(()),
                result = collect_frames(&mut reader) => result?,
            };
            return Err(ipc_status_error(status, &body));
        }
        let mut sse_frame = Vec::new();
        loop {
            let frame = tokio::select! {
                _ = cancellation.cancelled() => return Ok(()),
                frame = read_frame(&mut reader) => frame?,
            };
            match frame {
                IpcHttpResponseFrame::Chunk { data_base64 } => {
                    let chunk = BASE64.decode(data_base64).map_err(|error| {
                        ClientError::Daemon(format!("IPC response base64 is invalid: {error}"))
                    })?;
                    for byte in chunk {
                        sse_frame.push(byte);
                        if sse_frame.len() > MAX_SSE_EVENT_BYTES {
                            return Err(ClientError::Protocol(format!(
                                "SSE event exceeds {MAX_SSE_EVENT_BYTES} byte limit"
                            )));
                        }
                        if !sse_frame_complete(&sse_frame) {
                            continue;
                        }
                        let event = parse_sse_frame(&sse_frame)?;
                        sse_frame.clear();
                        let Some(event) = event else {
                            continue;
                        };
                        let Some(runtime_event) = super::decode_sse_runtime_event(event)? else {
                            continue;
                        };
                        if cursor
                            .is_some_and(|sequence_no| runtime_event.sequence_no <= sequence_no)
                        {
                            continue;
                        }
                        let sequence_no = runtime_event.sequence_no;
                        if sender.send(Ok(runtime_event)).await.is_err() {
                            return Ok(());
                        }
                        *cursor = Some(sequence_no);
                    }
                }
                IpcHttpResponseFrame::End => return Ok(()),
                IpcHttpResponseFrame::Error { message } => {
                    return Err(ClientError::Daemon(message));
                }
                IpcHttpResponseFrame::Head { .. } => {
                    return Err(ClientError::Daemon(
                        "IPC response contained a duplicate head".to_owned(),
                    ));
                }
            }
        }
    }
}

#[async_trait]
impl RuntimeClient for UnixIpcTransport {
    async fn send_command(&self, command: SessionCommand) -> Result<CommandAck, ClientError> {
        self.attached_json(
            "POST",
            "/commands",
            Some(
                encode_command_value_for_protocol(&command, self.protocol_version).map_err(
                    |error| ClientError::Daemon(format!("command wire encoding failed: {error}")),
                )?,
            ),
        )
        .await
    }

    async fn query(&self, query: RuntimeQuery) -> Result<Value, ClientError> {
        self.attached_json("POST", "/queries", Some(serde_json::to_value(query)?))
            .await
    }

    async fn event_page(&self, request: EventPageRequest) -> Result<EventPage, ClientError> {
        let mut pairs = vec![
            ("session_id", request.session_id.to_string()),
            (
                "direction",
                match request.direction {
                    EventPageDirection::Forward => "forward",
                    EventPageDirection::Backward => "backward",
                }
                .to_owned(),
            ),
            ("limit", request.limit.to_string()),
        ];
        if let Some(task_id) = request.task_id {
            pairs.push(("task_id", task_id.to_string()));
        }
        if let Some(cursor) = request.cursor {
            pairs.push(("cursor", cursor.to_string()));
        }
        self.attached_json("GET", &query_path("/events/page", pairs), None)
            .await
    }

    async fn replay_events(&self, filter: EventFilter) -> Result<Vec<Value>, ClientError> {
        let mut pairs = vec![("session_id", filter.session_id.to_string())];
        if let Some(task_id) = filter.task_id {
            pairs.push(("task_id", task_id.to_string()));
        }
        if let Some(cursor) = filter.after_sequence_no {
            pairs.push(("cursor", cursor.to_string()));
        }
        self.attached_json("GET", &query_path("/events/replay", pairs), None)
            .await
    }

    async fn subscribe(&self, filter: EventFilter) -> Result<RuntimeEventStream, ClientError> {
        self.ensure_open()?;
        let (sender, receiver) = mpsc::channel(256);
        let transport = self.clone();
        tokio::spawn(async move {
            transport.run_subscription(filter, sender).await;
        });
        Ok(RuntimeEventStream::new(receiver))
    }
}

#[async_trait]
impl TaskTraceClient for UnixIpcTransport {
    async fn task_trace(&self, request: TaskTraceRequest) -> Result<TaskTracePage, ClientError> {
        self.attached_json("POST", "/traces", Some(serde_json::to_value(request)?))
            .await
    }

    async fn read_artifact_chunk(
        &self,
        request: ArtifactReadRequest,
    ) -> Result<Option<ArtifactChunk>, ClientError> {
        self.attached_json(
            "POST",
            "/artifacts/chunk",
            Some(serde_json::to_value(request)?),
        )
        .await
    }
}

async fn exchange(
    socket_path: &Path,
    token: &SecretString,
    protocol_version: u32,
    attachment_id: Option<&str>,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Result<IpcResponse, ClientError> {
    let future = async {
        let (status, mut reader) = open_exchange(IpcExchange {
            socket_path,
            token,
            protocol_version,
            attachment_id,
            method,
            path,
            body,
        })
        .await?;
        let body = collect_frames(&mut reader).await?;
        Ok(IpcResponse { status, body })
    };
    tokio::time::timeout(Duration::from_secs(30), future)
        .await
        .map_err(|_| ClientError::Daemon("IPC request timed out".to_owned()))?
}

struct IpcExchange<'a> {
    socket_path: &'a Path,
    token: &'a SecretString,
    protocol_version: u32,
    attachment_id: Option<&'a str>,
    method: &'a str,
    path: &'a str,
    body: Option<Value>,
}

async fn open_exchange(
    request: IpcExchange<'_>,
) -> Result<(u16, BufReader<OwnedReadHalf>), ClientError> {
    open_exchange_with_cancellation(request, None).await
}

async fn open_exchange_with_cancellation(
    request: IpcExchange<'_>,
    cancellation: Option<&CancellationToken>,
) -> Result<(u16, BufReader<OwnedReadHalf>), ClientError> {
    let operation = async {
        let IpcExchange {
            socket_path,
            token,
            protocol_version,
            attachment_id,
            method,
            path,
            body,
        } = request;
        let mut headers = BTreeMap::from([
            (
                "authorization".to_owned(),
                format!("Bearer {}", token.expose_secret()),
            ),
            (
                APP_SERVER_PROTOCOL_HEADER.to_owned(),
                protocol_version.to_string(),
            ),
        ]);
        if let Some(attachment_id) = attachment_id {
            headers.insert(
                APP_SERVER_ATTACHMENT_HEADER.to_owned(),
                attachment_id.to_owned(),
            );
        }
        let request = IpcHttpRequest {
            method: method.to_owned(),
            path: path.to_owned(),
            headers,
            body,
        };
        let mut bytes = serde_json::to_vec(&request)?;
        if bytes.len() > MAX_WIRE_MESSAGE_BYTES {
            return Err(ClientError::Daemon(
                "IPC request exceeds its size limit".to_owned(),
            ));
        }
        bytes.push(b'\n');
        let stream = tokio::time::timeout(Duration::from_secs(2), UnixStream::connect(socket_path))
            .await
            .map_err(|_| ClientError::Daemon("IPC connection timed out".to_owned()))?
            .map_err(|error| ClientError::Daemon(error.to_string()))?;
        let (reader, mut writer) = stream.into_split();
        writer
            .write_all(&bytes)
            .await
            .map_err(|error| ClientError::Daemon(error.to_string()))?;
        writer
            .shutdown()
            .await
            .map_err(|error| ClientError::Daemon(error.to_string()))?;
        let mut reader = BufReader::new(reader);
        let head = read_response_head(&mut reader, IPC_RESPONSE_HEAD_TIMEOUT).await?;
        match head {
            IpcHttpResponseFrame::Head { status, .. } => Ok((status, reader)),
            IpcHttpResponseFrame::Error { message } => Err(ClientError::Daemon(message)),
            _ => Err(ClientError::Daemon(
                "IPC response did not start with a head frame".to_owned(),
            )),
        }
    };
    match cancellation {
        Some(cancellation) => {
            tokio::select! {
                _ = cancellation.cancelled() => {
                    Err(ClientError::Daemon("runtime transport is closed".to_owned()))
                }
                result = operation => result,
            }
        }
        None => operation.await,
    }
}

async fn read_response_head<R>(
    reader: &mut R,
    timeout: Duration,
) -> Result<IpcHttpResponseFrame, ClientError>
where
    R: AsyncBufRead + Unpin,
{
    tokio::time::timeout(timeout, read_frame(reader))
        .await
        .map_err(|_| ClientError::Daemon("IPC response head timed out".to_owned()))?
}

async fn collect_frames(reader: &mut BufReader<OwnedReadHalf>) -> Result<Vec<u8>, ClientError> {
    let mut body = Vec::new();
    loop {
        match read_frame(reader).await? {
            IpcHttpResponseFrame::Chunk { data_base64 } => {
                let chunk = BASE64.decode(data_base64).map_err(|error| {
                    ClientError::Protocol(format!("IPC response base64 is invalid: {error}"))
                })?;
                if body.len().saturating_add(chunk.len()) > MAX_HTTP_JSON_RESPONSE_BYTES {
                    return Err(ClientError::Daemon(format!(
                        "IPC response exceeds {MAX_HTTP_JSON_RESPONSE_BYTES} byte limit"
                    )));
                }
                body.extend_from_slice(&chunk);
            }
            IpcHttpResponseFrame::End => return Ok(body),
            IpcHttpResponseFrame::Error { message } => {
                return Err(ClientError::Daemon(message));
            }
            IpcHttpResponseFrame::Head { .. } => {
                return Err(ClientError::Daemon(
                    "IPC response contained a duplicate head".to_owned(),
                ));
            }
        }
    }
}

async fn read_frame<R>(reader: &mut R) -> Result<IpcHttpResponseFrame, ClientError>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = Vec::new();
    let read = read_bounded_line(reader, &mut line, MAX_IPC_FRAME_BYTES)
        .await
        .map_err(|error| ClientError::Daemon(error.to_string()))?;
    if read == 0 {
        return Err(ClientError::Daemon(
            "IPC response ended before its end frame".to_owned(),
        ));
    }
    if line.len() > MAX_IPC_FRAME_BYTES || !line.ends_with(b"\n") {
        return Err(ClientError::Daemon(
            "IPC response frame exceeds its size limit".to_owned(),
        ));
    }
    serde_json::from_slice(&line).map_err(ClientError::Serialization)
}

async fn read_bounded_line<R>(
    reader: &mut R,
    line: &mut Vec<u8>,
    limit: usize,
) -> std::io::Result<usize>
where
    R: AsyncBufRead + Unpin,
{
    line.clear();
    let mut read = 0_usize;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(read);
        }
        let remaining = limit.saturating_add(1).saturating_sub(line.len());
        if remaining == 0 {
            return Ok(read);
        }
        let available = &available[..available.len().min(remaining)];
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |index| index.saturating_add(1));
        line.extend_from_slice(&available[..consumed]);
        reader.consume(consumed);
        read = read.saturating_add(consumed);
        if newline.is_some() || line.len() > limit {
            return Ok(read);
        }
    }
}

fn decode_ipc_json<T: DeserializeOwned>(response: IpcResponse) -> Result<T, ClientError> {
    if !(200..300).contains(&response.status) {
        return Err(ipc_status_error(response.status, &response.body));
    }
    serde_json::from_slice(&response.body).map_err(ClientError::Serialization)
}

fn ipc_status_error(status: u16, body: &[u8]) -> ClientError {
    let message = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| String::from_utf8_lossy(body).to_string());
    ClientError::Daemon(format!("IPC HTTP {status}: {message}"))
}

fn query_path<K, V, I>(base: &str, pairs: I) -> String
where
    K: AsRef<str>,
    V: AsRef<str>,
    I: IntoIterator<Item = (K, V)>,
{
    let mut url = reqwest::Url::parse(&format!("http://localhost{base}"))
        .expect("fixed IPC base URL is valid");
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in pairs {
            query.append_pair(key.as_ref(), value.as_ref());
        }
    }
    match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_owned(),
    }
}

fn event_path(filter: &EventFilter, cursor: Option<u64>) -> String {
    let mut pairs = vec![("session_id", filter.session_id.to_string())];
    if let Some(task_id) = filter.task_id {
        pairs.push(("task_id", task_id.to_string()));
    }
    if let Some(cursor) = cursor {
        pairs.push(("cursor", cursor.to_string()));
    }
    query_path("/events", pairs)
}

async fn validate_socket(path: &Path) -> Result<(), ClientError> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    let metadata = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|error| ClientError::Daemon(format!("{}: {error}", path.display())))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_socket() {
        return Err(ClientError::Daemon(format!(
            "app-server IPC path is not a Unix socket: {}",
            path.display()
        )));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(ClientError::Daemon(format!(
            "app-server IPC socket must be owner-only: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[tokio::test]
    async fn frame_reader_accepts_a_complete_bounded_frame() {
        let frame = IpcHttpResponseFrame::End;
        let mut input = serde_json::to_vec(&frame).expect("frame JSON");
        input.extend_from_slice(b"\nremaining");
        let mut reader = BufReader::new(Cursor::new(input));

        assert_eq!(read_frame(&mut reader).await.expect("frame"), frame);
        assert_eq!(reader.fill_buf().await.expect("remaining"), b"remaining");
    }

    #[tokio::test]
    async fn frame_reader_rejects_before_buffering_an_unbounded_line() {
        let mut input = vec![b'x'; MAX_IPC_FRAME_BYTES + 32];
        input.push(b'\n');
        let mut reader = BufReader::new(Cursor::new(input));

        let error = read_frame(&mut reader).await.expect_err("oversized frame");

        assert!(matches!(
            error,
            ClientError::Daemon(message)
                if message == "IPC response frame exceeds its size limit"
        ));
        assert_eq!(reader.fill_buf().await.expect("remaining").len(), 32);
    }

    #[tokio::test]
    async fn response_head_timeout_is_bounded() {
        let (_writer, stream) = tokio::io::duplex(1);
        let mut reader = BufReader::new(stream);

        let error = read_response_head(&mut reader, Duration::from_millis(1))
            .await
            .expect_err("a missing response head must time out");
        assert!(matches!(
            error,
            ClientError::Daemon(message) if message == "IPC response head timed out"
        ));
    }

    #[tokio::test]
    async fn cancelling_open_exchange_interrupts_a_pending_response_head() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let socket_path = directory.path().join("runtime.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).expect("IPC listener");
        let (accepted_sender, accepted_receiver) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("connection");
            let _ = accepted_sender.send(());
            let _stream = stream;
            std::future::pending::<()>().await;
        });
        let cancellation = CancellationToken::new();
        let token = SecretString::from("a".repeat(64));
        let request = open_exchange_with_cancellation(
            IpcExchange {
                socket_path: &socket_path,
                token: &token,
                protocol_version: crate::RUNTIME_PROTOCOL_VERSION,
                attachment_id: Some("attachment"),
                method: "GET",
                path: "/events",
                body: None,
            },
            Some(&cancellation),
        );
        tokio::pin!(request);
        tokio::select! {
            result = &mut request => panic!("response head unexpectedly completed: {result:?}"),
            _ = accepted_receiver => {}
        }

        cancellation.cancel();
        let error = tokio::time::timeout(std::time::Duration::from_secs(1), &mut request)
            .await
            .expect("cancellation must interrupt the request")
            .expect_err("cancelled request should return an error");
        assert!(matches!(
            error,
            ClientError::Daemon(message) if message == "runtime transport is closed"
        ));

        server.abort();
        let _ = server.await;
    }
}
