use std::{
    collections::HashMap,
    convert::Infallible,
    fs::{self, File, OpenOptions},
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Path as AxumPath, Query, Request, State},
    http::{HeaderMap, StatusCode, header, uri::Authority},
    middleware::{self, Next},
    response::{
        Html, IntoResponse, Response,
        sse::{Event, Sse},
    },
    routing::{delete, get, post},
};
use fs2::FileExt;
use golutra_client::{
    APP_SERVER_ATTACHMENT_HEADER, APP_SERVER_PROTOCOL_HEADER, AgentEventProjector, AppServerInfo,
    AppServerPaths, ClientError, EmbeddedTransport, RuntimeAttachment, RuntimeOperation,
    RuntimeOperationClient, app_server_attachment_actor_id,
};
use golutra_core::{Actor, ActorKind, CommandId, SessionId, TaskId, ThreadId, TraceView};
use golutra_protocol::{
    AgentThreadRef, ArtifactChunk, ArtifactReadRequest, CommandAck, EventFilter, EventPage,
    EventPageRequest, MAX_WIRE_MESSAGE_BYTES, ProtocolVersionRange, RUNTIME_PROTOCOL_VERSION,
    RuntimeQuery, SessionPage, SessionPageRequest, SessionWindow, SessionWindowRequest,
    TaskTracePage, TaskTraceRequest, decode_command_value, encode_event_value_for_protocol,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, OnceCell};
use uuid::Uuid;

const MAX_ATTACHED_RUNTIMES: usize = 128;

mod attachment_registry;
mod ipc;
mod rpc;
mod transport_security;

use attachment_registry::{
    AttachedAttachment, AttachmentInsertError, AttachmentRegistry, DEFAULT_IDLE_TTL,
    DEFAULT_MAX_ATTACHMENTS,
};
use transport_security::TransportAuth;

#[derive(Debug, Clone)]
pub struct AppState {
    inner: Arc<AppStateInner>,
}

#[derive(Debug)]
struct AppStateInner {
    info: AppServerInfo,
    max_runtimes: usize,
    max_attachments: usize,
    runtime_home: Option<PathBuf>,
    transport_auth: TransportAuth,
    runtimes: Mutex<HashMap<PathBuf, Arc<OnceCell<AttachedRuntime>>>>,
    attachments: Mutex<AttachmentRegistry>,
    lifecycle: Mutex<()>,
}

#[derive(Debug, Clone)]
struct AttachedRuntime {
    transport: EmbeddedTransport,
}

impl AppState {
    pub fn new(info: AppServerInfo, transport_token: &str) -> miette::Result<Self> {
        Ok(Self::with_limits(
            info,
            TransportAuth::from_token(transport_token)?,
            MAX_ATTACHED_RUNTIMES,
            DEFAULT_MAX_ATTACHMENTS,
            None,
        ))
    }

    #[cfg(test)]
    fn with_runtime_limit(
        info: AppServerInfo,
        transport_token: &str,
        max_runtimes: usize,
    ) -> miette::Result<Self> {
        Ok(Self::from_auth(
            info,
            TransportAuth::from_token(transport_token)?,
            max_runtimes,
            DEFAULT_MAX_ATTACHMENTS,
            None,
        ))
    }

    fn with_limits(
        info: AppServerInfo,
        transport_auth: TransportAuth,
        max_runtimes: usize,
        max_attachments: usize,
        runtime_home: Option<PathBuf>,
    ) -> Self {
        Self::from_auth(
            info,
            transport_auth,
            max_runtimes,
            max_attachments,
            runtime_home,
        )
    }

    fn from_auth(
        info: AppServerInfo,
        transport_auth: TransportAuth,
        max_runtimes: usize,
        max_attachments: usize,
        runtime_home: Option<PathBuf>,
    ) -> Self {
        Self {
            inner: Arc::new(AppStateInner {
                info,
                max_runtimes: max_runtimes.max(1),
                max_attachments: max_attachments.max(1),
                runtime_home,
                transport_auth,
                runtimes: Mutex::new(HashMap::new()),
                attachments: Mutex::new(AttachmentRegistry::new(max_attachments, DEFAULT_IDLE_TTL)),
                lifecycle: Mutex::new(()),
            }),
        }
    }

    pub(crate) fn info(&self) -> &AppServerInfo {
        &self.inner.info
    }

    async fn attach_cwd(&self, cwd: impl AsRef<Path>) -> Result<RuntimeAttachment, ClientError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        if !cwd.as_ref().is_absolute() {
            return Err(ClientError::InvalidSession(format!(
                "runtime cwd must be absolute: {}",
                cwd.as_ref().display()
            )));
        }
        let cwd = cwd
            .as_ref()
            .canonicalize()
            .map_err(|error| ClientError::Io(format!("{}: {error}", cwd.as_ref().display())))?;
        self.prune_unreferenced_runtimes_locked().await;
        let runtime = {
            let mut runtimes = self.inner.runtimes.lock().await;
            if let Some(runtime) = runtimes.get(&cwd) {
                runtime.clone()
            } else {
                if runtimes.len() >= self.inner.max_runtimes {
                    return Err(ClientError::Daemon(format!(
                        "app-server runtime attachment limit {} reached",
                        self.inner.max_runtimes
                    )));
                }
                let runtime = Arc::new(OnceCell::new());
                runtimes.insert(cwd.clone(), runtime.clone());
                runtime
            }
        };
        let runtime_home = self.inner.runtime_home.clone();
        let attached = match runtime
            .get_or_try_init(|| async {
                let transport = match runtime_home.as_ref() {
                    Some(home) => EmbeddedTransport::from_home_and_cwd(home, &cwd).await?,
                    None => EmbeddedTransport::for_cwd(&cwd).await?,
                };
                Ok::<_, ClientError>(AttachedRuntime { transport })
            })
            .await
        {
            Ok(attached) => attached.clone(),
            Err(error) => {
                let mut runtimes = self.inner.runtimes.lock().await;
                if runtime.get().is_none()
                    && runtimes
                        .get(&cwd)
                        .is_some_and(|registered| Arc::ptr_eq(registered, &runtime))
                {
                    runtimes.remove(&cwd);
                }
                return Err(error);
            }
        };
        let runtime = match attached
            .transport
            .runtime_info(self.inner.info.base_url.clone())
            .await
        {
            Ok(runtime) => runtime,
            Err(error) => {
                self.prune_unreferenced_runtimes_locked().await;
                return Err(error);
            }
        };
        // Every attach is a distinct controller capability. The durable
        // runtime is shared by cwd, but the attachment id is the server-bound
        // actor identity used for control commands.
        let attachment_id = Uuid::now_v7().to_string();
        let actor = Actor {
            kind: ActorKind::Api,
            id: app_server_attachment_actor_id(&attachment_id),
        };
        let insert_result = self.inner.attachments.lock().await.insert(
            attachment_id.clone(),
            attached.transport,
            actor,
            cwd.clone(),
            std::time::Instant::now(),
        );
        if let Err(error) = insert_result {
            self.prune_unreferenced_runtimes_locked().await;
            return Err(match error {
                AttachmentInsertError::Capacity => ClientError::Daemon(format!(
                    "app-server attachment limit {} reached",
                    self.inner.max_attachments
                )),
                AttachmentInsertError::Duplicate => {
                    ClientError::Daemon("app-server generated a duplicate attachment id".to_owned())
                }
            });
        }
        Ok(RuntimeAttachment {
            attachment_id,
            runtime,
        })
    }

    async fn attached_transport(
        &self,
        headers: &HeaderMap,
    ) -> Result<AttachedAttachment, AppError> {
        let attachment_id = self.attachment_id_from_headers(headers)?;
        self.attached_attachment(attachment_id)
            .await
            .ok_or_else(|| AppError::Attachment("runtime attachment was not found".to_owned()))
    }

    async fn attached_transport_id(
        &self,
        attachment_id: &str,
    ) -> Result<AttachedAttachment, AppError> {
        self.attached_attachment(attachment_id)
            .await
            .ok_or_else(|| AppError::Attachment("runtime attachment was not found".to_owned()))
    }

    async fn detach_attachment(&self, attachment_id: &str) -> bool {
        let revocation = {
            let _lifecycle = self.inner.lifecycle.lock().await;
            self.inner
                .attachments
                .lock()
                .await
                .detach_attachment(attachment_id)
        };
        let Some(revocation) = revocation else {
            return false;
        };
        revocation.wait_idle().await;
        self.prune_unreferenced_runtimes().await;
        true
    }

    async fn attached_attachment(
        &self,
        attachment_id: &str,
    ) -> Option<attachment_registry::AttachedAttachment> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        let attachment = self
            .inner
            .attachments
            .lock()
            .await
            .attachment(attachment_id, std::time::Instant::now());
        self.prune_unreferenced_runtimes_locked().await;
        attachment
    }

    async fn prune_unreferenced_runtimes(&self) {
        let _lifecycle = self.inner.lifecycle.lock().await;
        self.prune_unreferenced_runtimes_locked().await;
    }

    async fn prune_unreferenced_runtimes_locked(&self) {
        self.inner
            .attachments
            .lock()
            .await
            .prune_expired_at(std::time::Instant::now());
        let referenced = self.inner.attachments.lock().await.runtime_keys();
        let entries = self
            .inner
            .runtimes
            .lock()
            .await
            .iter()
            .map(|(runtime_key, runtime)| (runtime_key.clone(), runtime.clone()))
            .collect::<Vec<_>>();
        let mut keep_active = std::collections::HashSet::new();
        for (runtime_key, runtime) in entries {
            if referenced.contains(&runtime_key) {
                continue;
            }
            if let Some(attached) = runtime.get()
                && attached.transport.has_active_work().await
            {
                keep_active.insert(runtime_key);
            }
        }
        self.inner.runtimes.lock().await.retain(|runtime_key, _| {
            referenced.contains(runtime_key) || keep_active.contains(runtime_key)
        });
    }

    fn attachment_id_from_headers<'a>(&self, headers: &'a HeaderMap) -> Result<&'a str, AppError> {
        headers
            .get(APP_SERVER_ATTACHMENT_HEADER)
            .ok_or_else(|| {
                AppError::Attachment("runtime attachment header is required".to_owned())
            })?
            .to_str()
            .map_err(|_| AppError::Attachment("runtime attachment header is invalid".to_owned()))
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/runtime/info", get(runtime_info))
        .route("/runtime/attach", post(attach_runtime))
        .route("/runtime/attach/{attachment_id}", delete(detach_runtime))
        .route("/rpc", post(rpc::http_rpc))
        .route("/rpc/ws", get(rpc::websocket_rpc))
        .route("/attach", get(attach_page))
        .route("/commands", post(send_command))
        .route("/queries", post(query_runtime))
        .route("/traces", post(task_trace))
        .route("/artifacts/chunk", post(read_artifact_chunk))
        .route("/events", get(events))
        .route("/agent/events", get(agent_events))
        .route("/events/page", get(event_page))
        .route("/events/replay", get(replay_events))
        .route("/threads", get(list_threads))
        .route("/sessions/page", post(session_page))
        .route("/sessions/window", post(session_window))
        .route("/sessions/{session_id}/thread", get(thread_for_session))
        .route("/threads/{thread_id}/resume", post(resume_thread))
        .route("/threads/{thread_id}/fork", post(fork_thread))
        .route(
            "/threads/{thread_id}/rollout/export",
            post(export_thread_rollout),
        )
        .route("/threads/{thread_id}/rebind", post(rebind_thread))
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(state, enforce_http_boundary))
        .layer(DefaultBodyLimit::max(MAX_WIRE_MESSAGE_BYTES))
}

pub async fn run(addr: SocketAddr) -> miette::Result<()> {
    validate_runtime_bind_addr(addr)?;
    let paths = AppServerPaths::global().map_err(|error| miette::miette!("{error}"))?;
    let runtime_home = paths.home.clone();
    let lease = AppServerLease::acquire(&paths)?;
    let transport_auth = TransportAuth::load_or_create(&paths)?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|error| miette::miette!("{error}"))?;
    let local_addr = listener
        .local_addr()
        .map_err(|error| miette::miette!("{error}"))?;
    #[cfg(unix)]
    let (ipc_listener, _ipc_guard) = bind_ipc_socket(&paths.ipc_socket)?;
    let info = AppServerInfo {
        instance_id: Uuid::now_v7().to_string(),
        pid: std::process::id(),
        base_url: format!("http://{local_addr}"),
        ipc_path: app_server_ipc_path(&paths),
        protocol_versions: ProtocolVersionRange::runtime(),
        started_at: chrono::Utc::now(),
    };
    lease.publish(&info)?;
    let app = router(AppState::from_auth(
        info,
        transport_auth,
        MAX_ATTACHED_RUNTIMES,
        DEFAULT_MAX_ATTACHMENTS,
        Some(runtime_home),
    ));
    #[cfg(unix)]
    {
        tokio::select! {
            result = axum::serve(listener, app.clone()) => {
                result.map_err(|error| miette::miette!("{error}"))
            }
            result = ipc::serve(ipc_listener, app) => {
                result.map_err(|error| miette::miette!("{error}"))
            }
        }
    }
    #[cfg(not(unix))]
    {
        axum::serve(listener, app)
            .await
            .map_err(|error| miette::miette!("{error}"))
    }
}

/// Run the same app-server JSON-RPC dispatcher over newline-delimited stdio.
/// This is intended for IDE bridges and local process supervisors. It does
/// not publish a TCP endpoint, but attached workspaces still use the normal
/// durable runtime storage under the Golutra home directory.
pub async fn run_stdio() -> miette::Result<()> {
    let paths = AppServerPaths::global().map_err(|error| miette::miette!("{error}"))?;
    let runtime_home = paths.home.clone();
    let transport_auth = TransportAuth::load_or_create(&paths)?;
    let info = AppServerInfo {
        instance_id: Uuid::now_v7().to_string(),
        pid: std::process::id(),
        base_url: "stdio://golutra-app-server".to_owned(),
        ipc_path: app_server_ipc_path(&paths),
        protocol_versions: ProtocolVersionRange::runtime(),
        started_at: chrono::Utc::now(),
    };
    let state = AppState::from_auth(
        info,
        transport_auth,
        MAX_ATTACHED_RUNTIMES,
        DEFAULT_MAX_ATTACHMENTS,
        Some(runtime_home),
    );
    rpc::serve_stdio(state)
        .await
        .map_err(|error| miette::miette!("{error}"))
}

#[cfg(unix)]
fn app_server_ipc_path(paths: &AppServerPaths) -> Option<String> {
    Some(paths.ipc_socket.to_string_lossy().to_string())
}

#[cfg(not(unix))]
fn app_server_ipc_path(_paths: &AppServerPaths) -> Option<String> {
    None
}

#[cfg(unix)]
fn bind_ipc_socket(path: &Path) -> miette::Result<(tokio::net::UnixListener, IpcSocketGuard)> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(miette::miette!(
                "app-server IPC path cannot be a symbolic link: {}",
                path.display()
            ));
        }
        Ok(metadata) if metadata.file_type().is_socket() => {
            fs::remove_file(path).map_err(|error| miette::miette!("{error}"))?;
        }
        Ok(_) => {
            return Err(miette::miette!(
                "app-server IPC path is not a socket: {}",
                path.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(miette::miette!("{error}")),
    }
    let listener =
        tokio::net::UnixListener::bind(path).map_err(|error| miette::miette!("{error}"))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| miette::miette!("{error}"))?;
    Ok((
        listener,
        IpcSocketGuard {
            path: path.to_path_buf(),
        },
    ))
}

#[cfg(unix)]
struct IpcSocketGuard {
    path: PathBuf,
}

#[cfg(unix)]
impl Drop for IpcSocketGuard {
    fn drop(&mut self) {
        use std::os::unix::fs::FileTypeExt;

        if fs::symlink_metadata(&self.path)
            .ok()
            .is_some_and(|metadata| metadata.file_type().is_socket())
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn validate_runtime_bind_addr(addr: SocketAddr) -> miette::Result<()> {
    if addr.ip().is_loopback() {
        return Ok(());
    }
    Err(miette::miette!(
        "runtime app-server must bind to a loopback address until transport authentication is configured: {addr}"
    ))
}

async fn enforce_http_boundary(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if !local_http_headers(request.headers()) {
        return Err(StatusCode::FORBIDDEN);
    }
    if matches!(request.uri().path(), "/health" | "/attach") {
        return Ok(next.run(request).await);
    }
    if !state.inner.transport_auth.authorizes(request.headers()) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    // Runtime info is the authenticated protocol-negotiation endpoint.  It
    // must be readable before a client knows which protocol header to send.
    if request.uri().path() == "/runtime/info" {
        return Ok(next.run(request).await);
    }
    let protocol_version = request
        .headers()
        .get(APP_SERVER_PROTOCOL_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u32>().ok());
    if !protocol_version.is_some_and(|version| ProtocolVersionRange::runtime().accepts(version)) {
        return Err(StatusCode::UPGRADE_REQUIRED);
    }
    Ok(next.run(request).await)
}

fn local_http_headers(headers: &HeaderMap) -> bool {
    let host_is_local = headers
        .get(header::HOST)
        .is_none_or(|value| value.to_str().ok().is_some_and(loopback_authority));
    let origin_is_local = headers
        .get(header::ORIGIN)
        .is_none_or(|value| value.to_str().ok().is_some_and(loopback_origin));
    host_is_local && origin_is_local
}

fn loopback_authority(value: &str) -> bool {
    value
        .parse::<Authority>()
        .ok()
        .is_some_and(|authority| loopback_host(authority.host()))
}

fn loopback_origin(value: &str) -> bool {
    value
        .parse::<axum::http::Uri>()
        .ok()
        .and_then(|uri| uri.host().map(ToOwned::to_owned))
        .is_some_and(|host| loopback_host(&host))
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

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn runtime_info(State(state): State<AppState>) -> Json<AppServerInfo> {
    Json(state.inner.info.clone())
}

#[derive(Debug, Deserialize)]
struct AttachRequest {
    cwd: PathBuf,
    protocol_version: u32,
}

async fn attach_runtime(
    State(state): State<AppState>,
    Json(request): Json<AttachRequest>,
) -> Result<Json<RuntimeAttachment>, AppError> {
    let supported_versions = ProtocolVersionRange::runtime();
    if !supported_versions.accepts(request.protocol_version) {
        return Err(AppError::Protocol(format!(
            "client runtime protocol {} is incompatible with server range {}..={}",
            request.protocol_version, supported_versions.minimum, supported_versions.current
        )));
    }
    Ok(Json(state.attach_cwd(request.cwd).await?))
}

async fn detach_runtime(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(attachment_id): AxumPath<String>,
) -> Result<StatusCode, AppError> {
    let header_attachment_id = state.attachment_id_from_headers(&headers)?;
    if header_attachment_id != attachment_id {
        return Err(AppError::Attachment(
            "attachment path does not match the capability header".to_owned(),
        ));
    }
    if state.detach_attachment(&attachment_id).await {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::Attachment(
            "runtime attachment was not found".to_owned(),
        ))
    }
}

async fn send_command(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(value): Json<Value>,
) -> Result<Json<CommandAck>, AppError> {
    let transport = state.attached_transport(&headers).await?;
    let actor = transport.actor.clone();
    let mut command = decode_command_value(value)
        .map_err(|error| AppError::InvalidPayload(format!("invalid command payload: {error}")))?;
    command.actor = actor;
    Ok(Json(
        transport
            .execute_operation(RuntimeOperation::SendCommand(command))
            .await?
            .into_command_ack()?,
    ))
}

async fn query_runtime(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(query): Json<RuntimeQuery>,
) -> Result<Json<Value>, AppError> {
    let transport = state.attached_transport(&headers).await?;
    Ok(Json(
        transport
            .execute_operation(RuntimeOperation::Query(query))
            .await?
            .into_query()?,
    ))
}

async fn task_trace(
    State(state): State<AppState>,
    local_ipc: Option<Extension<ipc::LocalIpcRequest>>,
    headers: HeaderMap,
    Json(request): Json<TaskTraceRequest>,
) -> Result<Json<TaskTracePage>, AppError> {
    if request.view == TraceView::Forensic && local_ipc.is_none() {
        return Err(AppError::Disclosure(
            "forensic trace access requires the owner-only local IPC transport".to_owned(),
        ));
    }
    let transport = state.attached_transport(&headers).await?;
    Ok(Json(
        transport
            .execute_operation(RuntimeOperation::TaskTrace(request))
            .await?
            .into_task_trace()?,
    ))
}

async fn read_artifact_chunk(
    State(state): State<AppState>,
    local_ipc: Option<Extension<ipc::LocalIpcRequest>>,
    headers: HeaderMap,
    Json(request): Json<ArtifactReadRequest>,
) -> Result<Json<Option<ArtifactChunk>>, AppError> {
    let transport = state.attached_transport(&headers).await?;
    let chunk = transport
        .execute_operation(RuntimeOperation::ReadArtifactChunk(request))
        .await?
        .into_artifact_chunk()?;
    enforce_artifact_disclosure(local_ipc.is_some(), chunk.as_ref())?;
    Ok(Json(chunk))
}

fn enforce_artifact_disclosure(
    local_ipc: bool,
    chunk: Option<&ArtifactChunk>,
) -> Result<(), AppError> {
    if !local_ipc
        && chunk.is_some_and(|chunk| chunk.redaction_status == golutra_core::RedactionStatus::Raw)
    {
        return Err(AppError::Disclosure(
            "raw artifact access requires the owner-only local IPC transport".to_owned(),
        ));
    }
    Ok(())
}

async fn attach_page() -> Html<&'static str> {
    Html(ATTACH_PAGE)
}

async fn events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<EventQuery>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, AppError> {
    let attachment = state.attached_transport(&headers).await?;
    let transport = &*attachment;
    let cancellation = attachment.cancellation();
    let protocol_version = requested_protocol_version(&headers);
    let session_id = parse_session_id(&query.session_id)?;
    let task_id = query.task_id.as_deref().map(parse_task_id).transpose()?;
    let header_cursor = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let mut events = transport
        .execute_operation(RuntimeOperation::Subscribe(EventFilter {
            session_id,
            task_id,
            after_sequence_no: query.cursor.or(header_cursor),
        }))
        .await?
        .into_subscription()?;
    let stream = async_stream::stream! {
        let _attachment = attachment;
        loop {
            let event = tokio::select! {
                _ = cancellation.cancelled() => break,
                event = events.recv() => event,
            };
            let Some(event) = event else { break; };
            match event {
                Ok(event) => match encode_event_value_for_protocol(&event, protocol_version) {
                    Ok(value) => yield Ok::<Event, Infallible>(sse_event(value)),
                    Err(error) => {
                        yield Ok::<Event, Infallible>(sse_named_event(
                            "error",
                            json!({"error": error.to_string()}),
                        ));
                    }
                },
                Err(error) => {
                    yield Ok::<Event, Infallible>(sse_named_event(
                        "error",
                        json!({"error": error.to_string()}),
                    ));
                    break;
                }
            }
        }
    };
    Ok(Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default()))
}

/// Stream the normalized Agent contract over SSE.  The raw `/events` route is
/// still available for audit consumers; this route is for SDKs and clients
/// that should not reimplement RuntimeEvent projection in another language.
async fn agent_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AgentEventQuery>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, AppError> {
    let attachment = state.attached_transport(&headers).await?;
    let transport = &*attachment;
    let cancellation = attachment.cancellation();
    let session_id = parse_session_id(&query.session_id)?;
    let thread = if let Some(thread_id) = query.thread_id.as_deref() {
        let record = transport.resume_thread(parse_thread_id(thread_id)?).await?;
        AgentThreadRef {
            thread_id: record.thread_id,
            session_id: record.session_id,
            workspace_root: record.workspace_root,
        }
    } else {
        let record = transport.thread_for_session(session_id).await?;
        let record = record
            .ok_or_else(|| AppError::InvalidId(format!("session `{session_id}` has no thread")))?;
        AgentThreadRef {
            thread_id: record.thread_id,
            session_id: record.session_id,
            workspace_root: record.workspace_root,
        }
    };
    if thread.session_id != session_id {
        return Err(AppError::InvalidId(
            "thread and session do not belong together".to_owned(),
        ));
    }
    let command_id = query
        .command_id
        .as_deref()
        .map(parse_command_id)
        .transpose()?;
    let header_cursor = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let cursor = query.cursor.or(header_cursor);
    let projection_cursor = query.start_cursor.or(cursor);
    if let (Some(projection_cursor), Some(cursor)) = (projection_cursor, cursor)
        && projection_cursor > cursor
    {
        return Err(AppError::InvalidId(
            "start_cursor cannot be newer than cursor".to_owned(),
        ));
    }
    let mut events = transport
        .execute_operation(RuntimeOperation::Subscribe(EventFilter {
            session_id,
            task_id: None,
            after_sequence_no: projection_cursor,
        }))
        .await?
        .into_subscription()?;
    let mut projector = AgentEventProjector::new(thread, command_id);
    let stream = async_stream::stream! {
        let _attachment = attachment;
        // This transport lifecycle marker is emitted once per SSE connection,
        // including reconnects that carry a runtime cursor. Consumers must
        // therefore treat thread.started as idempotent.
        let initial = projector.thread_started();
        if let Ok(value) = serde_json::to_value(initial) {
            yield Ok::<Event, Infallible>(agent_sse_event(value));
        }
        loop {
            let event = tokio::select! {
                _ = cancellation.cancelled() => break,
                event = events.recv() => event,
            };
            let Some(event) = event else { break; };
            match event {
                Ok(event) => {
                    let sequence_no = event.sequence_no;
                    let Some(projected) = projector.project(event) else {
                        continue;
                    };
                    let terminal = projector.is_finished();
                    if cursor.is_some_and(|cursor| sequence_no <= cursor) {
                        if terminal {
                            break;
                        }
                        continue;
                    }
                    match serde_json::to_value(projected) {
                        Ok(value) => {
                            yield Ok::<Event, Infallible>(agent_sse_event(value));
                        }
                        Err(error) => {
                            yield Ok::<Event, Infallible>(sse_named_event(
                                "error",
                                json!({"error": error.to_string()}),
                            ));
                            break;
                        }
                    }
                    if terminal {
                        break;
                    }
                }
                Err(error) => {
                    yield Ok::<Event, Infallible>(sse_named_event(
                        "error",
                        json!({"error": error.to_string()}),
                    ));
                    break;
                }
            }
        }
    };
    Ok(Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default()))
}

fn sse_event(value: Value) -> Event {
    let sequence_no = value
        .get("event")
        .and_then(|event| event.get("sequence_no"))
        .and_then(Value::as_u64)
        .or_else(|| value.get("sequence_no").and_then(Value::as_u64));
    let builder = sequence_no.map_or_else(Event::default, |sequence_no| {
        Event::default().id(sequence_no.to_string())
    });
    builder.json_data(value).unwrap_or_else(|_| {
        Event::default().data(json!({"error": "event serialization failed"}).to_string())
    })
}

fn requested_protocol_version(headers: &HeaderMap) -> u32 {
    headers
        .get(APP_SERVER_PROTOCOL_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(RUNTIME_PROTOCOL_VERSION)
}

fn sse_named_event(name: &'static str, value: Value) -> Event {
    Event::default()
        .event(name)
        .json_data(value)
        .unwrap_or_else(|_| {
            Event::default()
                .event("error")
                .data("event serialization failed")
        })
}

fn agent_sse_event(value: Value) -> Event {
    let sequence_no = value
        .pointer("/event/sequence_no")
        .or_else(|| value.pointer("/item/sequence_no"))
        .or_else(|| value.get("last_sequence_no"))
        .and_then(Value::as_u64);
    let builder = sequence_no.map_or_else(Event::default, |sequence_no| {
        Event::default().id(sequence_no.to_string())
    });
    builder
        .event("agent_event")
        .json_data(value)
        .unwrap_or_else(|_| {
            Event::default().data(json!({"error": "event serialization failed"}).to_string())
        })
}

async fn replay_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<EventQuery>,
) -> Result<Json<Vec<Value>>, AppError> {
    let transport = state.attached_transport(&headers).await?;
    Ok(Json(
        transport
            .execute_operation(RuntimeOperation::ReplayEvents(event_filter(query)?))
            .await?
            .into_replayed_events()?,
    ))
}

async fn event_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(request): Query<EventPageRequest>,
) -> Result<Json<EventPage>, AppError> {
    let transport = state.attached_transport(&headers).await?;
    Ok(Json(
        transport
            .execute_operation(RuntimeOperation::EventPage(request))
            .await?
            .into_event_page()?,
    ))
}

#[derive(Debug, Deserialize)]
struct ThreadQuery {
    limit: Option<u32>,
}

async fn list_threads(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ThreadQuery>,
) -> Result<Json<Vec<golutra_store::ThreadRecord>>, AppError> {
    let transport = state.attached_transport(&headers).await?;
    Ok(Json(
        transport.list_threads(query.limit.unwrap_or(20)).await?,
    ))
}

async fn session_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<SessionPageRequest>,
) -> Result<Json<SessionPage>, AppError> {
    let transport = state.attached_transport(&headers).await?;
    Ok(Json(transport.session_page(request).await?))
}

async fn session_window(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<SessionWindowRequest>,
) -> Result<Json<SessionWindow>, AppError> {
    let transport = state.attached_transport(&headers).await?;
    Ok(Json(transport.session_window(request).await?))
}

async fn thread_for_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(session_id): AxumPath<String>,
) -> Result<Json<Option<golutra_store::ThreadRecord>>, AppError> {
    let transport = state.attached_transport(&headers).await?;
    Ok(Json(
        transport
            .thread_for_session(parse_session_id(&session_id)?)
            .await?,
    ))
}

async fn resume_thread(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(thread_id): AxumPath<String>,
) -> Result<Json<golutra_store::ThreadRecord>, AppError> {
    let transport = state.attached_transport(&headers).await?;
    Ok(Json(
        transport
            .resume_thread(parse_thread_id(&thread_id)?)
            .await?,
    ))
}

async fn fork_thread(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(thread_id): AxumPath<String>,
    Json(request): Json<ForkThreadRequest>,
) -> Result<Json<golutra_store::ThreadRecord>, AppError> {
    let transport = state.attached_transport(&headers).await?;
    Ok(Json(
        transport
            .fork_thread(parse_thread_id(&thread_id)?, request.from_turn_id)
            .await?,
    ))
}

#[derive(Debug, Deserialize)]
struct ForkThreadRequest {
    from_turn_id: Option<golutra_core::TurnId>,
}

async fn export_thread_rollout(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(thread_id): AxumPath<String>,
) -> Result<Json<golutra_client::RolloutExport>, AppError> {
    let transport = state.attached_transport(&headers).await?;
    Ok(Json(
        transport
            .export_thread_rollout(parse_thread_id(&thread_id)?)
            .await?,
    ))
}

#[derive(Debug, Deserialize)]
struct RebindThreadRequest {
    from_workspace_root: String,
}

async fn rebind_thread(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(thread_id): AxumPath<String>,
    Json(request): Json<RebindThreadRequest>,
) -> Result<Json<golutra_client::ThreadRebindResult>, AppError> {
    let transport = state.attached_transport(&headers).await?;
    Ok(Json(
        transport
            .rebind_thread(parse_thread_id(&thread_id)?, request.from_workspace_root)
            .await?,
    ))
}

#[derive(Debug, Deserialize)]
struct EventQuery {
    session_id: String,
    task_id: Option<String>,
    cursor: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct AgentEventQuery {
    session_id: String,
    thread_id: Option<String>,
    command_id: Option<String>,
    start_cursor: Option<u64>,
    cursor: Option<u64>,
}

fn event_filter(query: EventQuery) -> Result<EventFilter, AppError> {
    Ok(EventFilter {
        session_id: parse_session_id(&query.session_id)?,
        task_id: query.task_id.as_deref().map(parse_task_id).transpose()?,
        after_sequence_no: query.cursor,
    })
}

#[derive(Debug)]
enum AppError {
    Client(ClientError),
    InvalidId(String),
    InvalidPayload(String),
    Attachment(String),
    Protocol(String),
    Disclosure(String),
}

impl From<ClientError> for AppError {
    fn from(error: ClientError) -> Self {
        Self::Client(error)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::Client(ClientError::InvalidSession(error)) => (StatusCode::BAD_REQUEST, error),
            Self::Client(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
            Self::InvalidId(error) => (StatusCode::BAD_REQUEST, error),
            Self::InvalidPayload(error) => (StatusCode::BAD_REQUEST, error),
            Self::Attachment(error) => (StatusCode::GONE, error),
            Self::Protocol(error) => (StatusCode::UPGRADE_REQUIRED, error),
            Self::Disclosure(error) => (StatusCode::FORBIDDEN, error),
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

fn parse_session_id(value: &str) -> Result<SessionId, AppError> {
    Uuid::from_str(value)
        .map(SessionId)
        .map_err(|_| AppError::InvalidId(format!("invalid session_id: {value}")))
}

fn parse_task_id(value: &str) -> Result<TaskId, AppError> {
    Uuid::from_str(value)
        .map(TaskId)
        .map_err(|_| AppError::InvalidId(format!("invalid task_id: {value}")))
}

fn parse_thread_id(value: &str) -> Result<ThreadId, AppError> {
    value
        .parse()
        .map_err(|_| AppError::InvalidId(format!("invalid thread_id: {value}")))
}

fn parse_command_id(value: &str) -> Result<CommandId, AppError> {
    value
        .parse()
        .map_err(|_| AppError::InvalidId(format!("invalid command_id: {value}")))
}

struct AppServerLease {
    _lock: File,
    endpoint_path: PathBuf,
    instance_id: std::sync::Mutex<Option<String>>,
}

impl AppServerLease {
    fn acquire(paths: &AppServerPaths) -> miette::Result<Self> {
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&paths.lock)
            .map_err(|error| miette::miette!("{error}"))?;
        set_owner_only_file(&lock)?;
        lock.try_lock_exclusive()
            .map_err(|error| miette::miette!("Golutra app-server is already running: {error}"))?;
        Ok(Self {
            _lock: lock,
            endpoint_path: paths.endpoint.clone(),
            instance_id: std::sync::Mutex::new(None),
        })
    }

    fn publish(&self, info: &AppServerInfo) -> miette::Result<()> {
        let parent = self
            .endpoint_path
            .parent()
            .ok_or_else(|| miette::miette!("app-server endpoint path has no parent"))?;
        let mut temporary =
            tempfile::NamedTempFile::new_in(parent).map_err(|error| miette::miette!("{error}"))?;
        temporary
            .write_all(
                &serde_json::to_vec_pretty(info).map_err(|error| miette::miette!("{error}"))?,
            )
            .map_err(|error| miette::miette!("{error}"))?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|error| miette::miette!("{error}"))?;
        set_owner_only_file(temporary.as_file())?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|error| miette::miette!("{error}"))?;
        temporary
            .persist(&self.endpoint_path)
            .map_err(|error| miette::miette!("{error}"))?;
        sync_app_server_directory(parent)?;
        *self
            .instance_id
            .lock()
            .map_err(|_| miette::miette!("app-server lease lock is poisoned"))? =
            Some(info.instance_id.clone());
        Ok(())
    }
}

impl Drop for AppServerLease {
    fn drop(&mut self) {
        let own_instance = self
            .instance_id
            .lock()
            .ok()
            .and_then(|instance| instance.clone());
        let endpoint_instance = fs::read(&self.endpoint_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<AppServerInfo>(&bytes).ok())
            .map(|info| info.instance_id);
        if own_instance.is_some()
            && own_instance == endpoint_instance
            && fs::remove_file(&self.endpoint_path).is_ok()
            && let Some(parent) = self.endpoint_path.parent()
        {
            let _ = sync_app_server_directory(parent);
        }
    }
}

#[cfg(unix)]
fn sync_app_server_directory(path: &Path) -> miette::Result<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| miette::miette!("{error}"))
}

#[cfg(not(unix))]
fn sync_app_server_directory(_path: &Path) -> miette::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_file(file: &File) -> miette::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|error| miette::miette!("{error}"))
}

#[cfg(not(unix))]
fn set_owner_only_file(_file: &File) -> miette::Result<()> {
    Ok(())
}

const ATTACH_PAGE: &str = r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>Golutra Runtime</title>
    <style>
      :root { color-scheme: light dark; font-family: ui-sans-serif, system-ui, sans-serif; }
      body { margin: 0; background: Canvas; color: CanvasText; }
      main { width: min(1080px, calc(100vw - 32px)); margin: 24px auto; display: grid; gap: 12px; }
      form { display: grid; grid-template-columns: 2fr 1fr 1fr auto; gap: 8px; align-items: end; }
      label { display: grid; gap: 4px; font-size: 12px; }
      input, button { min-height: 36px; font: inherit; }
      pre { min-height: 420px; overflow: auto; padding: 12px; border: 1px solid GrayText; border-radius: 6px; white-space: pre-wrap; word-break: break-word; }
      @media (max-width: 720px) { form { grid-template-columns: 1fr; } }
    </style>
  </head>
  <body>
    <main>
      <form id="attach-form">
        <label>CWD<input id="cwd" required autocomplete="off" /></label>
        <label>Session ID<input id="session-id" required autocomplete="off" /></label>
        <label>Transport token<input id="transport-token" type="password" required autocomplete="off" /></label>
        <button type="submit">Attach</button>
      </form>
      <pre id="output" aria-live="polite"></pre>
    </main>
    <script>
      const form = document.getElementById("attach-form");
      const output = document.getElementById("output");
      let controller;

      form.addEventListener("submit", async (event) => {
        event.preventDefault();
        controller?.abort();
        controller = new AbortController();
        output.textContent = "";
        const cwd = document.getElementById("cwd").value.trim();
        const sessionId = document.getElementById("session-id").value.trim();
        const transportToken = document.getElementById("transport-token").value.trim();
        const authHeaders = { "authorization": `Bearer ${transportToken}` };
        const infoResponse = await fetch("/runtime/info", { headers: authHeaders });
        if (!infoResponse.ok) {
          output.textContent = `runtime discovery failed: ${infoResponse.status} ${await infoResponse.text()}`;
          return;
        }
        const runtimeInfo = await infoResponse.json();
        const protocolVersion = runtimeInfo.protocol_versions?.current;
        if (!Number.isInteger(protocolVersion)) {
          output.textContent = "runtime discovery returned an invalid protocol version";
          return;
        }
        const transportHeaders = {
          ...authHeaders,
          "x-golutra-protocol-version": String(protocolVersion)
        };
        const attached = await fetch("/runtime/attach", {
          method: "POST",
          headers: { ...transportHeaders, "content-type": "application/json" },
          body: JSON.stringify({ cwd, protocol_version: protocolVersion })
        });
        if (!attached.ok) {
          output.textContent = `attach failed: ${attached.status} ${await attached.text()}`;
          return;
        }
        const attachment = await attached.json();
        const response = await fetch(`/events?session_id=${encodeURIComponent(sessionId)}`, {
          headers: {
            ...transportHeaders,
            "accept": "text/event-stream",
            "x-golutra-attachment": attachment.attachment_id
          },
          signal: controller.signal
        });
        if (!response.ok || !response.body) {
          output.textContent = `stream failed: ${response.status} ${await response.text()}`;
          return;
        }
        const reader = response.body.getReader();
        const decoder = new TextDecoder();
        let buffer = "";
        while (true) {
          const { value, done } = await reader.read();
          if (done) break;
          buffer += decoder.decode(value, { stream: true });
          const frames = buffer.split("\n\n");
          buffer = frames.pop() || "";
          for (const frame of frames) {
            const data = frame.split("\n").filter((line) => line.startsWith("data:"));
            for (const line of data) output.textContent += `${line.slice(5).trim()}\n`;
          }
        }
      });
    </script>
  </body>
</html>
"#;

#[cfg(test)]
mod tests {
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use futures_util::StreamExt;
    use golutra_core::{Actor, ActorKind, ArtifactId, CommandId, RedactionStatus};
    use golutra_protocol::{
        RUNTIME_PROTOCOL_VERSION, RuntimeQueryKind, SessionCommand, SessionCommandKind,
    };
    use tower::ServiceExt;

    use super::*;

    const TEST_TRANSPORT_TOKEN: &str =
        "test-transport-token-000000000000000000000000000000000000000000000000";

    fn server_info() -> AppServerInfo {
        AppServerInfo {
            instance_id: Uuid::now_v7().to_string(),
            pid: std::process::id(),
            base_url: "http://127.0.0.1:0".to_owned(),
            ipc_path: None,
            protocol_versions: ProtocolVersionRange::runtime(),
            started_at: chrono::Utc::now(),
        }
    }

    fn authorized_request(builder: axum::http::request::Builder) -> axum::http::request::Builder {
        builder
            .header(
                header::AUTHORIZATION,
                format!("Bearer {TEST_TRANSPORT_TOKEN}"),
            )
            .header(APP_SERVER_PROTOCOL_HEADER, RUNTIME_PROTOCOL_VERSION)
    }

    async fn state_with_attachment_and_transport()
    -> (AppState, String, SessionId, EmbeddedTransport) {
        let state = AppState::new(server_info(), TEST_TRANSPORT_TOKEN).expect("app state");
        let transport = EmbeddedTransport::in_memory().await.expect("transport");
        let attachment_id = Uuid::now_v7().to_string();
        let session_id = transport.default_session_id();
        state
            .inner
            .attachments
            .lock()
            .await
            .insert(
                attachment_id.clone(),
                transport.clone(),
                Actor {
                    kind: ActorKind::Api,
                    id: format!("test-attachment-{attachment_id}"),
                },
                PathBuf::from("/test-workspace"),
                std::time::Instant::now(),
            )
            .expect("attachment insert");
        (state, attachment_id, session_id, transport)
    }

    async fn state_with_attachment() -> (AppState, String, SessionId) {
        let (state, attachment_id, session_id, _) = state_with_attachment_and_transport().await;
        (state, attachment_id, session_id)
    }

    #[tokio::test]
    async fn runtime_attachment_rejects_relative_cwd() {
        let state = AppState::new(server_info(), TEST_TRANSPORT_TOKEN).expect("app state");

        let error = state
            .attach_cwd("relative-workspace")
            .await
            .expect_err("relative cwd must be rejected");

        assert!(matches!(error, ClientError::InvalidSession(_)));
    }

    #[tokio::test]
    async fn runtime_attachment_uses_the_home_captured_by_the_server() {
        let home = tempfile::tempdir().expect("runtime home");
        let workspace = tempfile::tempdir().expect("workspace");
        let state = AppState::from_auth(
            server_info(),
            TransportAuth::from_token(TEST_TRANSPORT_TOKEN).expect("transport auth"),
            MAX_ATTACHED_RUNTIMES,
            DEFAULT_MAX_ATTACHMENTS,
            Some(home.path().to_path_buf()),
        );

        state
            .attach_cwd(workspace.path())
            .await
            .expect("attach runtime");

        assert!(home.path().join("state/runtime.sqlite").is_file());
    }

    #[tokio::test]
    async fn runtime_attachment_registry_is_bounded_and_failed_slots_are_released() {
        let state = AppState::with_runtime_limit(server_info(), TEST_TRANSPORT_TOKEN, 1)
            .expect("app state");
        let transport = EmbeddedTransport::in_memory().await.expect("transport");
        let occupied = Arc::new(OnceCell::new());
        occupied
            .set(AttachedRuntime { transport })
            .expect("runtime cell");
        state
            .inner
            .attachments
            .lock()
            .await
            .insert(
                "occupied-attachment".to_owned(),
                occupied.get().expect("occupied runtime").transport.clone(),
                Actor {
                    kind: ActorKind::Api,
                    id: "occupied-actor".to_owned(),
                },
                PathBuf::from("/already-attached"),
                std::time::Instant::now(),
            )
            .expect("occupied attachment");
        state
            .inner
            .runtimes
            .lock()
            .await
            .insert(PathBuf::from("/already-attached"), occupied);
        let workspace = tempfile::tempdir().expect("workspace");

        let error = state
            .attach_cwd(workspace.path())
            .await
            .expect_err("runtime limit must be enforced");

        assert!(matches!(error, ClientError::Daemon(_)));

        state.inner.runtimes.lock().await.clear();
        let invalid_cwd = workspace.path().join("not-a-directory");
        fs::write(&invalid_cwd, "file").expect("invalid cwd fixture");
        state
            .attach_cwd(&invalid_cwd)
            .await
            .expect_err("file cwd must fail initialization");
        assert!(state.inner.runtimes.lock().await.is_empty());
    }

    #[tokio::test]
    async fn detaching_the_last_attachment_releases_the_runtime_slot() {
        let home = tempfile::tempdir().expect("runtime home");
        let first_workspace = tempfile::tempdir().expect("first workspace");
        let second_workspace = tempfile::tempdir().expect("second workspace");
        let state = AppState::from_auth(
            server_info(),
            TransportAuth::from_token(TEST_TRANSPORT_TOKEN).expect("transport auth"),
            1,
            DEFAULT_MAX_ATTACHMENTS,
            Some(home.path().to_path_buf()),
        );

        let first = state
            .attach_cwd(first_workspace.path())
            .await
            .expect("first attachment");
        let duplicate = state
            .attach_cwd(first_workspace.path())
            .await
            .expect("second attachment to the same runtime");
        assert_eq!(state.inner.runtimes.lock().await.len(), 1);

        assert!(state.detach_attachment(&first.attachment_id).await);
        assert_eq!(state.inner.runtimes.lock().await.len(), 1);
        assert!(state.detach_attachment(&duplicate.attachment_id).await);
        assert!(state.inner.runtimes.lock().await.is_empty());

        state
            .attach_cwd(second_workspace.path())
            .await
            .expect("released runtime slot can be reused");
    }

    #[tokio::test]
    async fn expired_attachment_releases_its_idle_runtime_slot_on_next_attach() {
        let home = tempfile::tempdir().expect("runtime home");
        let first_workspace = tempfile::tempdir().expect("first workspace");
        let second_workspace = tempfile::tempdir().expect("second workspace");
        let state = AppState::from_auth(
            server_info(),
            TransportAuth::from_token(TEST_TRANSPORT_TOKEN).expect("transport auth"),
            1,
            DEFAULT_MAX_ATTACHMENTS,
            Some(home.path().to_path_buf()),
        );
        let first_runtime_key = first_workspace.path().canonicalize().expect("first cwd");
        let stale_runtime = Arc::new(OnceCell::new());
        stale_runtime
            .set(AttachedRuntime {
                transport: EmbeddedTransport::in_memory().await.expect("transport"),
            })
            .expect("runtime cell");
        state
            .inner
            .attachments
            .lock()
            .await
            .insert(
                "stale-attachment".to_owned(),
                stale_runtime
                    .get()
                    .expect("stale runtime")
                    .transport
                    .clone(),
                Actor {
                    kind: ActorKind::Api,
                    id: "stale-actor".to_owned(),
                },
                first_runtime_key.clone(),
                std::time::Instant::now()
                    .checked_sub(DEFAULT_IDLE_TTL + std::time::Duration::from_secs(1))
                    .expect("stale timestamp"),
            )
            .expect("stale attachment");
        state
            .inner
            .runtimes
            .lock()
            .await
            .insert(first_runtime_key, stale_runtime);

        state
            .attach_cwd(second_workspace.path())
            .await
            .expect("expired runtime slot is reusable");

        let runtimes = state.inner.runtimes.lock().await;
        assert_eq!(runtimes.len(), 1);
        assert!(
            runtimes.contains_key(&second_workspace.path().canonicalize().expect("second cwd"))
        );
    }

    #[tokio::test]
    async fn detaching_an_active_task_keeps_the_runtime_available_for_reattach() {
        let home = tempfile::tempdir().expect("runtime home");
        let workspace = tempfile::tempdir().expect("workspace");
        let state = AppState::from_auth(
            server_info(),
            TransportAuth::from_token(TEST_TRANSPORT_TOKEN).expect("transport auth"),
            1,
            DEFAULT_MAX_ATTACHMENTS,
            Some(home.path().to_path_buf()),
        );
        let first = state
            .attach_cwd(workspace.path())
            .await
            .expect("first attachment");
        let attachment = state
            .attached_transport_id(&first.attachment_id)
            .await
            .expect("attached transport");
        let transport = attachment.transport.clone();
        drop(attachment);
        let ack = transport
            .execute_operation(RuntimeOperation::SendCommand(
                golutra_protocol::SessionCommand {
                    command_id: CommandId::new(),
                    session_id: Some(first.runtime.default_session_id),
                    kind: golutra_protocol::SessionCommandKind::Prompt,
                    idempotency_key: CommandId::new().to_string(),
                    actor: Actor {
                        kind: ActorKind::Api,
                        id: "active-task-owner".to_owned(),
                    },
                    payload: json!({"prompt": "sleep"}),
                    timestamp: chrono::Utc::now(),
                },
            ))
            .await
            .expect("start task")
            .into_command_ack()
            .expect("command ack");
        assert!(ack.accepted);
        assert!(transport.has_active_work().await);

        assert!(state.detach_attachment(&first.attachment_id).await);
        assert_eq!(state.inner.runtimes.lock().await.len(), 1);
        let second = state
            .attach_cwd(workspace.path())
            .await
            .expect("reattach active runtime");
        assert_eq!(second.runtime.instance_id, first.runtime.instance_id);
        let abort = transport
            .execute_operation(RuntimeOperation::SendCommand(
                golutra_protocol::SessionCommand {
                    command_id: CommandId::new(),
                    session_id: Some(first.runtime.default_session_id),
                    kind: golutra_protocol::SessionCommandKind::Abort,
                    idempotency_key: CommandId::new().to_string(),
                    actor: Actor {
                        kind: ActorKind::Api,
                        id: "active-task-owner".to_owned(),
                    },
                    payload: json!({}),
                    timestamp: chrono::Utc::now(),
                },
            ))
            .await
            .expect("abort task")
            .into_command_ack()
            .expect("abort ack");
        assert!(abort.accepted);

        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while transport.has_active_work().await {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("task completion");
        assert!(state.detach_attachment(&second.attachment_id).await);
        assert!(state.inner.runtimes.lock().await.is_empty());
    }

    #[tokio::test]
    async fn attachment_capacity_failure_does_not_retain_an_unowned_runtime() {
        let home = tempfile::tempdir().expect("runtime home");
        let first_workspace = tempfile::tempdir().expect("first workspace");
        let second_workspace = tempfile::tempdir().expect("second workspace");
        let state = AppState::from_auth(
            server_info(),
            TransportAuth::from_token(TEST_TRANSPORT_TOKEN).expect("transport auth"),
            2,
            1,
            Some(home.path().to_path_buf()),
        );

        state
            .attach_cwd(first_workspace.path())
            .await
            .expect("first attachment");
        let error = state
            .attach_cwd(second_workspace.path())
            .await
            .expect_err("attachment capacity");
        assert!(matches!(error, ClientError::Daemon(_)));
        assert_eq!(state.inner.runtimes.lock().await.len(), 1);
    }

    #[test]
    fn runtime_server_rejects_non_loopback_bind_addresses() {
        assert!(validate_runtime_bind_addr("127.0.0.1:0".parse().expect("IPv4")).is_ok());
        assert!(validate_runtime_bind_addr("[::1]:0".parse().expect("IPv6")).is_ok());
        assert!(validate_runtime_bind_addr("0.0.0.0:47831".parse().expect("wildcard")).is_err());
    }

    #[test]
    fn browser_boundary_rejects_non_loopback_host_and_origin() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:47831".parse().expect("host"));
        headers.insert(
            header::ORIGIN,
            "http://localhost:47831".parse().expect("origin"),
        );
        assert!(local_http_headers(&headers));
        headers.insert(
            header::ORIGIN,
            "https://example.com".parse().expect("origin"),
        );
        assert!(!local_http_headers(&headers));
    }

    #[tokio::test]
    async fn runtime_info_is_server_scoped() {
        let info = server_info();
        let app = router(AppState::new(info.clone(), TEST_TRANSPORT_TOKEN).expect("app state"));
        let response = app
            .oneshot(
                authorized_request(Request::builder().uri("/runtime/info"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        assert_eq!(
            serde_json::from_slice::<AppServerInfo>(&body).expect("info"),
            info
        );
    }

    #[tokio::test]
    async fn runtime_protocol_endpoints_require_bearer_auth_and_version() {
        let app = router(AppState::new(server_info(), TEST_TRANSPORT_TOKEN).expect("app state"));
        let unauthorized = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/runtime/info")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let wrong_token = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/runtime/info")
                    .header(header::AUTHORIZATION, format!("Bearer {}", "x".repeat(64)))
                    .header(APP_SERVER_PROTOCOL_HEADER, RUNTIME_PROTOCOL_VERSION)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(wrong_token.status(), StatusCode::UNAUTHORIZED);

        let missing_version = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/runtime/attach")
                    .header(
                        header::AUTHORIZATION,
                        format!("Bearer {TEST_TRANSPORT_TOKEN}"),
                    )
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(missing_version.status(), StatusCode::UPGRADE_REQUIRED);
    }

    #[tokio::test]
    async fn authenticated_runtime_info_supports_protocol_discovery() {
        let app = router(AppState::new(server_info(), TEST_TRANSPORT_TOKEN).expect("app state"));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/runtime/info")
                    .header(
                        header::AUTHORIZATION,
                        format!("Bearer {TEST_TRANSPORT_TOKEN}"),
                    )
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn http_routes_reject_bodies_larger_than_the_wire_protocol_limit() {
        let app = router(AppState::new(server_info(), TEST_TRANSPORT_TOKEN).expect("app state"));
        let response = app
            .oneshot(
                authorized_request(
                    Request::builder()
                        .method("POST")
                        .uri("/rpc")
                        .header(header::CONTENT_TYPE, "application/json"),
                )
                .body(Body::from(vec![b' '; MAX_WIRE_MESSAGE_BYTES + 1]))
                .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn remote_http_rejects_forensic_trace_disclosure() {
        let app = router(AppState::new(server_info(), TEST_TRANSPORT_TOKEN).expect("app state"));
        let request = TaskTraceRequest {
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            view: TraceView::Forensic,
            cursor: None,
            limit: 64,
            wait_for_evaluation: false,
        };
        let response = app
            .oneshot(
                authorized_request(
                    Request::builder()
                        .method("POST")
                        .uri("/traces")
                        .header(header::CONTENT_TYPE, "application/json"),
                )
                .body(Body::from(serde_json::to_vec(&request).expect("json")))
                .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn remote_artifact_reads_reject_raw_blobs_but_allow_redacted_chunks() {
        let raw = ArtifactChunk {
            artifact_id: ArtifactId::new(),
            offset: 0,
            length: 1,
            total_size: 1,
            checksum: "sha256:raw".to_owned(),
            redaction_status: RedactionStatus::Raw,
            content_base64: "eA==".to_owned(),
            eof: true,
        };
        assert!(matches!(
            enforce_artifact_disclosure(false, Some(&raw)),
            Err(AppError::Disclosure(_))
        ));
        let mut redacted = raw.clone();
        redacted.redaction_status = RedactionStatus::Redacted;
        assert!(enforce_artifact_disclosure(false, Some(&redacted)).is_ok());
        assert!(enforce_artifact_disclosure(true, Some(&raw)).is_ok());
    }

    #[tokio::test]
    async fn command_endpoint_requires_valid_attachment() {
        let (state, attachment_id, session_id) = state_with_attachment().await;
        let command = SessionCommand {
            command_id: CommandId::new(),
            session_id: Some(session_id),
            kind: SessionCommandKind::Create,
            idempotency_key: CommandId::new().to_string(),
            actor: Actor {
                kind: ActorKind::Sdk,
                id: "test".to_owned(),
            },
            payload: json!({}),
            timestamp: chrono::Utc::now(),
        };
        let app = router(state);
        let missing = app
            .clone()
            .oneshot(
                authorized_request(
                    Request::builder()
                        .method("POST")
                        .uri("/commands")
                        .header(header::CONTENT_TYPE, "application/json"),
                )
                .body(Body::from(serde_json::to_vec(&command).expect("json")))
                .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(missing.status(), StatusCode::GONE);

        let accepted = app
            .oneshot(
                authorized_request(
                    Request::builder()
                        .method("POST")
                        .uri("/commands")
                        .header(header::CONTENT_TYPE, "application/json")
                        .header(APP_SERVER_ATTACHMENT_HEADER, attachment_id),
                )
                .body(Body::from(serde_json::to_vec(&command).expect("json")))
                .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(accepted.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn event_sse_uses_the_negotiated_v7_and_v8_wire_shapes() {
        let (state, attachment_id, session_id, transport) =
            state_with_attachment_and_transport().await;
        let app = router(state);
        let subscribe = |protocol_version| {
            app.clone().oneshot(
                Request::builder()
                    .uri(format!("/events?session_id={session_id}"))
                    .header(
                        header::AUTHORIZATION,
                        format!("Bearer {TEST_TRANSPORT_TOKEN}"),
                    )
                    .header(APP_SERVER_PROTOCOL_HEADER, protocol_version)
                    .header(APP_SERVER_ATTACHMENT_HEADER, &attachment_id)
                    .body(Body::empty())
                    .expect("event request"),
            )
        };
        let legacy_response = subscribe(7).await.expect("legacy SSE response");
        let current_response = subscribe(8).await.expect("current SSE response");
        assert_eq!(legacy_response.status(), StatusCode::OK);
        assert_eq!(current_response.status(), StatusCode::OK);

        let command = SessionCommand {
            command_id: CommandId::new(),
            session_id: Some(session_id),
            kind: SessionCommandKind::Create,
            idempotency_key: CommandId::new().to_string(),
            actor: Actor {
                kind: ActorKind::Api,
                id: "sse-version-test".to_owned(),
            },
            payload: json!({}),
            timestamp: chrono::Utc::now(),
        };
        let ack = transport
            .execute_operation(RuntimeOperation::SendCommand(command))
            .await
            .expect("emit runtime event")
            .into_command_ack()
            .expect("command ack");
        assert!(ack.accepted);

        let legacy = first_sse_data(legacy_response.into_body()).await;
        assert!(legacy.get("codec_version").is_none());
        assert!(legacy.get("event").is_none());
        assert!(legacy.get("schema_version").is_some());

        let current = first_sse_data(current_response.into_body()).await;
        assert_eq!(current["codec_version"], 1);
        assert_eq!(current["payload_kind"], "event");
        assert!(current.pointer("/event/schema_version").is_some());
    }

    async fn first_sse_data(body: Body) -> Value {
        tokio::time::timeout(std::time::Duration::from_secs(5), async move {
            let mut stream = body.into_data_stream();
            let mut pending = String::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.expect("SSE body chunk");
                pending.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(frame_end) = pending.find("\n\n") {
                    let frame = pending[..frame_end].to_owned();
                    pending.drain(..frame_end + 2);
                    if let Some(data) = frame.lines().find_map(|line| line.strip_prefix("data: ")) {
                        return serde_json::from_str(data).expect("SSE JSON data");
                    }
                }
            }
            panic!("SSE stream ended before an event arrived");
        })
        .await
        .expect("SSE event timeout")
    }

    #[tokio::test]
    async fn typed_operations_match_embedded_and_http_results() {
        let (state, attachment_id, session_id, transport) =
            state_with_attachment_and_transport().await;
        let mut events = transport
            .execute_operation(RuntimeOperation::Subscribe(EventFilter {
                session_id,
                task_id: None,
                after_sequence_no: None,
            }))
            .await
            .expect("embedded subscription")
            .into_subscription()
            .expect("subscription result");
        let command = SessionCommand {
            command_id: CommandId::new(),
            session_id: Some(session_id),
            kind: SessionCommandKind::Prompt,
            idempotency_key: CommandId::new().to_string(),
            actor: Actor {
                kind: ActorKind::Sdk,
                id: "parity-test".to_owned(),
            },
            payload: json!({"prompt": "typed operation parity"}),
            timestamp: chrono::Utc::now(),
        };
        let app = router(state);
        let response = app
            .clone()
            .oneshot(
                authorized_request(
                    Request::builder()
                        .method("POST")
                        .uri("/commands")
                        .header(header::CONTENT_TYPE, "application/json")
                        .header(APP_SERVER_ATTACHMENT_HEADER, &attachment_id),
                )
                .body(Body::from(serde_json::to_vec(&command).expect("json")))
                .expect("command request"),
            )
            .await
            .expect("command response");
        assert_eq!(response.status(), StatusCode::OK);
        let ack = serde_json::from_slice::<CommandAck>(
            &to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("command body"),
        )
        .expect("command ack");
        assert!(ack.accepted);

        let task_id = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let mut task_id = None;
            while let Some(event) = events.recv().await {
                let event = event.expect("runtime event");
                task_id = task_id.or(event.task_id);
                if event.event_type.is_task_terminal() {
                    return task_id.expect("terminal task id");
                }
            }
            panic!("runtime event stream ended before task completion");
        })
        .await
        .expect("task completion");

        let trace_request = TaskTraceRequest {
            session_id,
            task_id,
            view: TraceView::Full,
            cursor: None,
            limit: 512,
            wait_for_evaluation: true,
        };
        let embedded_trace = transport
            .execute_operation(RuntimeOperation::TaskTrace(trace_request.clone()))
            .await
            .expect("embedded trace")
            .into_task_trace()
            .expect("trace result");

        let query = RuntimeQuery {
            query_id: golutra_core::QueryId::new(),
            session_id,
            task_id: Some(task_id),
            kind: RuntimeQueryKind::SessionState,
            requester: ActorKind::Api,
            cursor: None,
            timestamp: chrono::Utc::now(),
        };
        let embedded_query = transport
            .execute_operation(RuntimeOperation::Query(query.clone()))
            .await
            .expect("embedded query")
            .into_query()
            .expect("query result");
        let response = app
            .clone()
            .oneshot(
                authorized_request(
                    Request::builder()
                        .method("POST")
                        .uri("/queries")
                        .header(header::CONTENT_TYPE, "application/json")
                        .header(APP_SERVER_ATTACHMENT_HEADER, &attachment_id),
                )
                .body(Body::from(serde_json::to_vec(&query).expect("query json")))
                .expect("query request"),
            )
            .await
            .expect("query response");
        assert_eq!(response.status(), StatusCode::OK);
        let http_query = serde_json::from_slice::<Value>(
            &to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("query body"),
        )
        .expect("query value");
        assert_eq!(http_query, embedded_query);

        let response = app
            .oneshot(
                authorized_request(
                    Request::builder()
                        .method("POST")
                        .uri("/traces")
                        .header(header::CONTENT_TYPE, "application/json")
                        .header(APP_SERVER_ATTACHMENT_HEADER, &attachment_id),
                )
                .body(Body::from(
                    serde_json::to_vec(&trace_request).expect("trace json"),
                ))
                .expect("trace request"),
            )
            .await
            .expect("trace response");
        assert_eq!(response.status(), StatusCode::OK);
        let http_trace = serde_json::from_slice::<TaskTracePage>(
            &to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("trace body"),
        )
        .expect("trace page");
        assert_eq!(http_trace, embedded_trace);
    }

    #[tokio::test]
    async fn detach_endpoint_revokes_the_attachment_capability() {
        let (state, attachment_id, session_id) = state_with_attachment().await;
        let app = router(state);
        let missing_header = app
            .clone()
            .oneshot(
                authorized_request(
                    Request::builder()
                        .method("DELETE")
                        .uri(format!("/runtime/attach/{attachment_id}")),
                )
                .body(Body::empty())
                .expect("missing-header detach request"),
            )
            .await
            .expect("missing-header detach response");
        assert_eq!(missing_header.status(), StatusCode::GONE);

        let detached = app
            .clone()
            .oneshot(
                authorized_request(
                    Request::builder()
                        .method("DELETE")
                        .uri(format!("/runtime/attach/{attachment_id}"))
                        .header(APP_SERVER_ATTACHMENT_HEADER, &attachment_id),
                )
                .body(Body::empty())
                .expect("detach request"),
            )
            .await
            .expect("detach response");
        assert_eq!(detached.status(), StatusCode::NO_CONTENT);

        let command = SessionCommand {
            command_id: CommandId::new(),
            session_id: Some(session_id),
            kind: SessionCommandKind::Create,
            idempotency_key: CommandId::new().to_string(),
            actor: Actor {
                kind: ActorKind::Sdk,
                id: "test".to_owned(),
            },
            payload: json!({}),
            timestamp: chrono::Utc::now(),
        };
        let revoked = app
            .clone()
            .oneshot(
                authorized_request(
                    Request::builder()
                        .method("POST")
                        .uri("/commands")
                        .header(header::CONTENT_TYPE, "application/json")
                        .header(APP_SERVER_ATTACHMENT_HEADER, &attachment_id),
                )
                .body(Body::from(serde_json::to_vec(&command).expect("json")))
                .expect("command request"),
            )
            .await
            .expect("command response");
        assert_eq!(revoked.status(), StatusCode::GONE);

        let already_detached = app
            .oneshot(
                authorized_request(
                    Request::builder()
                        .method("DELETE")
                        .uri(format!("/runtime/attach/{attachment_id}"))
                        .header(APP_SERVER_ATTACHMENT_HEADER, &attachment_id),
                )
                .body(Body::empty())
                .expect("second detach request"),
            )
            .await
            .expect("second detach response");
        assert_eq!(already_detached.status(), StatusCode::GONE);
    }

    #[tokio::test]
    async fn attach_page_is_served() {
        let app = router(AppState::new(server_info(), TEST_TRANSPORT_TOKEN).expect("app state"));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/attach")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains("x-golutra-attachment"));
        assert!(body.contains("runtimeInfo.protocol_versions?.current"));
        assert!(!body.contains("\"x-golutra-protocol-version\": \"2\""));
    }
}
