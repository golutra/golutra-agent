use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use golutra_protocol::{IpcHttpRequest, IpcHttpResponseFrame};
use secrecy::{ExposeSecret, SecretString};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixStream, unix::OwnedReadHalf},
    sync::mpsc,
};

use super::*;

const MAX_IPC_FRAME_BYTES: usize = 128 * 1024;
const MAX_IPC_REQUEST_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
struct IpcResponse {
    status: u16,
    body: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct UnixIpcTransport {
    pub(crate) socket_path: PathBuf,
    pub(crate) server_info: AppServerInfo,
    pub(crate) info: RuntimeHostInfo,
    pub(crate) cwd: PathBuf,
    pub(crate) attachment_id: Arc<RwLock<String>>,
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
        if bytes.len() > MAX_IPC_REQUEST_BYTES {
            return Err(ClientError::Daemon(
                "app-server endpoint metadata exceeds its size limit".to_owned(),
            ));
        }
        let endpoint: AppServerInfo = serde_json::from_slice(&bytes)?;
        validate_local_app_server_base_url(&endpoint.base_url)?;
        if !endpoint.protocol_versions.accepts(RUNTIME_PROTOCOL_VERSION) {
            return Err(ClientError::Daemon(format!(
                "runtime protocol {} is incompatible with server range {}..={}",
                RUNTIME_PROTOCOL_VERSION,
                endpoint.protocol_versions.minimum,
                endpoint.protocol_versions.current,
            )));
        }
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
        let attachment: RuntimeAttachment = decode_ipc_json(
            exchange(
                &socket_path,
                &transport_token,
                None,
                "POST",
                "/runtime/attach",
                Some(json!({
                    "cwd": paths.cwd,
                    "protocol_version": RUNTIME_PROTOCOL_VERSION,
                })),
            )
            .await?,
        )?;
        let attached_cwd = PathBuf::from(&attachment.runtime.cwd);
        if attached_cwd != paths.cwd {
            return Err(ClientError::Daemon(format!(
                "app-server attached `{}` instead of local cwd `{}`",
                attached_cwd.display(),
                paths.cwd.display()
            )));
        }
        Ok(Self {
            socket_path,
            server_info,
            info: attachment.runtime,
            cwd: attached_cwd,
            attachment_id: Arc::new(RwLock::new(attachment.attachment_id)),
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
        let attachment_id = self.current_attachment_id()?;
        let response = exchange(
            &self.socket_path,
            &self.transport_token,
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
            Some(&attachment_id),
            method,
            path,
            body,
        )
        .await
    }

    fn current_attachment_id(&self) -> Result<String, ClientError> {
        self.attachment_id
            .read()
            .map(|attachment_id| attachment_id.clone())
            .map_err(|_| ClientError::Daemon("runtime attachment lock is poisoned".to_owned()))
    }

    pub(crate) fn current_attachment_actor_id(&self) -> Result<String, ClientError> {
        self.current_attachment_id()
            .map(|id| app_server_attachment_actor_id(&id))
    }

    async fn refresh_attachment(&self, stale_attachment_id: &str) -> Result<String, ClientError> {
        let current = self.current_attachment_id()?;
        if current != stale_attachment_id {
            return Ok(current);
        }
        let attachment: RuntimeAttachment = decode_ipc_json(
            exchange(
                &self.socket_path,
                &self.transport_token,
                None,
                "POST",
                "/runtime/attach",
                Some(json!({
                    "cwd": self.cwd,
                    "protocol_version": RUNTIME_PROTOCOL_VERSION,
                })),
            )
            .await?,
        )?;
        let attached_cwd = PathBuf::from(&attachment.runtime.cwd);
        if attached_cwd != self.cwd {
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
        Ok(attachment_id)
    }

    async fn run_subscription(
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
                .consume_subscription(&filter, &mut cursor, &sender)
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

    async fn consume_subscription(
        &self,
        filter: &EventFilter,
        cursor: &mut Option<u64>,
        sender: &mpsc::Sender<Result<RuntimeEvent, ClientError>>,
    ) -> Result<(), ClientError> {
        let path = event_path(filter, *cursor);
        let attachment_id = self.current_attachment_id()?;
        let (mut status, mut reader) = open_exchange(
            &self.socket_path,
            &self.transport_token,
            Some(&attachment_id),
            "GET",
            &path,
            None,
        )
        .await?;
        if status == 410 {
            let attachment_id = self.refresh_attachment(&attachment_id).await?;
            (status, reader) = open_exchange(
                &self.socket_path,
                &self.transport_token,
                Some(&attachment_id),
                "GET",
                &path,
                None,
            )
            .await?;
        }
        if !(200..300).contains(&status) {
            let body = collect_frames(&mut reader).await?;
            return Err(ipc_status_error(status, &body));
        }
        let mut sse_frame = Vec::new();
        loop {
            match read_frame(&mut reader).await? {
                IpcHttpResponseFrame::Chunk { data_base64 } => {
                    let chunk = BASE64.decode(data_base64).map_err(|error| {
                        ClientError::Daemon(format!("IPC response base64 is invalid: {error}"))
                    })?;
                    for byte in chunk {
                        sse_frame.push(byte);
                        if sse_frame.len() > MAX_SSE_EVENT_BYTES {
                            return Err(ClientError::Daemon(format!(
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
                        if event.event == "lag" {
                            continue;
                        }
                        if event.event == "error" {
                            return Err(ClientError::Daemon(event.data));
                        }
                        let runtime_event: RuntimeEvent = serde_json::from_str(&event.data)?;
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
        self.attached_json("POST", "/commands", Some(serde_json::to_value(command)?))
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
    attachment_id: Option<&str>,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Result<IpcResponse, ClientError> {
    let future = async {
        let (status, mut reader) =
            open_exchange(socket_path, token, attachment_id, method, path, body).await?;
        let body = collect_frames(&mut reader).await?;
        Ok(IpcResponse { status, body })
    };
    tokio::time::timeout(Duration::from_secs(30), future)
        .await
        .map_err(|_| ClientError::Daemon("IPC request timed out".to_owned()))?
}

async fn open_exchange(
    socket_path: &Path,
    token: &SecretString,
    attachment_id: Option<&str>,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Result<(u16, BufReader<OwnedReadHalf>), ClientError> {
    let mut headers = BTreeMap::from([
        (
            "authorization".to_owned(),
            format!("Bearer {}", token.expose_secret()),
        ),
        (
            APP_SERVER_PROTOCOL_HEADER.to_owned(),
            RUNTIME_PROTOCOL_VERSION.to_string(),
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
    bytes.push(b'\n');
    if bytes.len() > MAX_IPC_REQUEST_BYTES {
        return Err(ClientError::Daemon(
            "IPC request exceeds its size limit".to_owned(),
        ));
    }
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
    match read_frame(&mut reader).await? {
        IpcHttpResponseFrame::Head { status, .. } => Ok((status, reader)),
        IpcHttpResponseFrame::Error { message } => Err(ClientError::Daemon(message)),
        _ => Err(ClientError::Daemon(
            "IPC response did not start with a head frame".to_owned(),
        )),
    }
}

async fn collect_frames(reader: &mut BufReader<OwnedReadHalf>) -> Result<Vec<u8>, ClientError> {
    let mut body = Vec::new();
    loop {
        match read_frame(reader).await? {
            IpcHttpResponseFrame::Chunk { data_base64 } => {
                let chunk = BASE64.decode(data_base64).map_err(|error| {
                    ClientError::Daemon(format!("IPC response base64 is invalid: {error}"))
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

async fn read_frame(
    reader: &mut BufReader<OwnedReadHalf>,
) -> Result<IpcHttpResponseFrame, ClientError> {
    let mut line = Vec::new();
    let read = reader
        .read_until(b'\n', &mut line)
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
