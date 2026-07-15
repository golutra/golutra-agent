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
    Json, Router,
    extract::{Path as AxumPath, Query, Request, State},
    http::{HeaderMap, StatusCode, header, uri::Authority},
    middleware::{self, Next},
    response::{
        Html, IntoResponse, Response,
        sse::{Event, Sse},
    },
    routing::{get, post},
};
use fs2::FileExt;
use golutra_client::{
    APP_SERVER_ATTACHMENT_HEADER, APP_SERVER_PROTOCOL_HEADER, AppServerInfo, AppServerPaths,
    ClientError, EmbeddedTransport, RuntimeAttachment, RuntimeClient,
};
use golutra_core::{SessionId, TaskId, ThreadId};
use golutra_protocol::{
    CommandAck, EventFilter, EventPage, EventPageRequest, ProtocolVersionRange,
    RUNTIME_PROTOCOL_VERSION, RuntimeQuery, SessionCommand,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, OnceCell};
use uuid::Uuid;

const MAX_ATTACHED_RUNTIMES: usize = 128;

mod ipc;
mod transport_security;

use transport_security::TransportAuth;

#[derive(Debug, Clone)]
pub struct AppState {
    inner: Arc<AppStateInner>,
}

#[derive(Debug)]
struct AppStateInner {
    info: AppServerInfo,
    max_runtimes: usize,
    transport_auth: TransportAuth,
    runtimes: Mutex<HashMap<PathBuf, Arc<OnceCell<AttachedRuntime>>>>,
    attachments: Mutex<HashMap<String, EmbeddedTransport>>,
}

#[derive(Debug, Clone)]
struct AttachedRuntime {
    attachment_id: String,
    transport: EmbeddedTransport,
}

impl AppState {
    pub fn new(info: AppServerInfo, transport_token: &str) -> miette::Result<Self> {
        Self::with_runtime_limit(info, transport_token, MAX_ATTACHED_RUNTIMES)
    }

    fn with_runtime_limit(
        info: AppServerInfo,
        transport_token: &str,
        max_runtimes: usize,
    ) -> miette::Result<Self> {
        Ok(Self::from_auth(
            info,
            TransportAuth::from_token(transport_token)?,
            max_runtimes,
        ))
    }

    fn from_auth(info: AppServerInfo, transport_auth: TransportAuth, max_runtimes: usize) -> Self {
        Self {
            inner: Arc::new(AppStateInner {
                info,
                max_runtimes: max_runtimes.max(1),
                transport_auth,
                runtimes: Mutex::new(HashMap::new()),
                attachments: Mutex::new(HashMap::new()),
            }),
        }
    }

    async fn attach_cwd(&self, cwd: impl AsRef<Path>) -> Result<RuntimeAttachment, ClientError> {
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
        let attached = match runtime
            .get_or_try_init(|| async {
                let transport = EmbeddedTransport::for_cwd(&cwd).await?;
                Ok::<_, ClientError>(AttachedRuntime {
                    attachment_id: Uuid::now_v7().to_string(),
                    transport,
                })
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
        let runtime = attached
            .transport
            .runtime_info(self.inner.info.base_url.clone())
            .await?;
        self.inner
            .attachments
            .lock()
            .await
            .insert(attached.attachment_id.clone(), attached.transport);
        Ok(RuntimeAttachment {
            attachment_id: attached.attachment_id,
            runtime,
        })
    }

    async fn attached_transport(&self, headers: &HeaderMap) -> Result<EmbeddedTransport, AppError> {
        let attachment_id = headers
            .get(APP_SERVER_ATTACHMENT_HEADER)
            .ok_or_else(|| {
                AppError::Attachment("runtime attachment header is required".to_owned())
            })?
            .to_str()
            .map_err(|_| AppError::Attachment("runtime attachment header is invalid".to_owned()))?;
        self.inner
            .attachments
            .lock()
            .await
            .get(attachment_id)
            .cloned()
            .ok_or_else(|| AppError::Attachment("runtime attachment was not found".to_owned()))
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/runtime/info", get(runtime_info))
        .route("/runtime/attach", post(attach_runtime))
        .route("/attach", get(attach_page))
        .route("/commands", post(send_command))
        .route("/queries", post(query_runtime))
        .route("/events", get(events))
        .route("/events/page", get(event_page))
        .route("/events/replay", get(replay_events))
        .route("/threads", get(list_threads))
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
}

pub async fn run(addr: SocketAddr) -> miette::Result<()> {
    validate_runtime_bind_addr(addr)?;
    let paths = AppServerPaths::global().map_err(|error| miette::miette!("{error}"))?;
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
    if request.protocol_version != RUNTIME_PROTOCOL_VERSION {
        return Err(AppError::Protocol(format!(
            "client runtime protocol {} is incompatible with server protocol {}",
            request.protocol_version, RUNTIME_PROTOCOL_VERSION
        )));
    }
    Ok(Json(state.attach_cwd(request.cwd).await?))
}

async fn send_command(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(command): Json<SessionCommand>,
) -> Result<Json<CommandAck>, AppError> {
    let transport = state.attached_transport(&headers).await?;
    Ok(Json(transport.send_command(command).await?))
}

async fn query_runtime(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(query): Json<RuntimeQuery>,
) -> Result<Json<Value>, AppError> {
    let transport = state.attached_transport(&headers).await?;
    Ok(Json(transport.query(query).await?))
}

async fn attach_page() -> Html<&'static str> {
    Html(ATTACH_PAGE)
}

async fn events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<EventQuery>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, AppError> {
    let transport = state.attached_transport(&headers).await?;
    let session_id = parse_session_id(&query.session_id)?;
    let task_id = query.task_id.as_deref().map(parse_task_id).transpose()?;
    let header_cursor = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let mut events = transport
        .subscribe(EventFilter {
            session_id,
            task_id,
            after_sequence_no: query.cursor.or(header_cursor),
        })
        .await?;
    let stream = async_stream::stream! {
        while let Some(event) = events.recv().await {
            match event {
                Ok(event) => match serde_json::to_value(event) {
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

fn sse_event(value: Value) -> Event {
    let sequence_no = value.get("sequence_no").and_then(Value::as_u64);
    let builder = sequence_no.map_or_else(Event::default, |sequence_no| {
        Event::default().id(sequence_no.to_string())
    });
    builder.json_data(value).unwrap_or_else(|_| {
        Event::default().data(json!({"error": "event serialization failed"}).to_string())
    })
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

async fn replay_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<EventQuery>,
) -> Result<Json<Vec<Value>>, AppError> {
    let transport = state.attached_transport(&headers).await?;
    Ok(Json(transport.replay_events(event_filter(query)?).await?))
}

async fn event_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(request): Query<EventPageRequest>,
) -> Result<Json<EventPage>, AppError> {
    let transport = state.attached_transport(&headers).await?;
    Ok(Json(transport.event_page(request).await?))
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
    Attachment(String),
    Protocol(String),
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
            Self::Attachment(error) => (StatusCode::GONE, error),
            Self::Protocol(error) => (StatusCode::UPGRADE_REQUIRED, error),
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
        const transportHeaders = {
          "authorization": `Bearer ${transportToken}`,
          "x-golutra-protocol-version": "1"
        };
        const attached = await fetch("/runtime/attach", {
          method: "POST",
          headers: { ...transportHeaders, "content-type": "application/json" },
          body: JSON.stringify({ cwd, protocol_version: 1 })
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
    use golutra_core::{Actor, ActorKind, CommandId};
    use golutra_protocol::SessionCommandKind;
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

    async fn state_with_attachment() -> (AppState, String, SessionId) {
        let state = AppState::new(server_info(), TEST_TRANSPORT_TOKEN).expect("app state");
        let transport = EmbeddedTransport::in_memory().await.expect("transport");
        let attachment_id = Uuid::now_v7().to_string();
        let session_id = transport.default_session_id();
        state
            .inner
            .attachments
            .lock()
            .await
            .insert(attachment_id.clone(), transport);
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
    async fn runtime_attachment_registry_is_bounded_and_failed_slots_are_released() {
        let state = AppState::with_runtime_limit(server_info(), TEST_TRANSPORT_TOKEN, 1)
            .expect("app state");
        let transport = EmbeddedTransport::in_memory().await.expect("transport");
        let occupied = Arc::new(OnceCell::new());
        occupied
            .set(AttachedRuntime {
                attachment_id: "existing".to_owned(),
                transport,
            })
            .expect("runtime cell");
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
        assert_eq!(missing_version.status(), StatusCode::UPGRADE_REQUIRED);
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
        assert!(String::from_utf8_lossy(&body).contains("x-golutra-attachment"));
    }
}
