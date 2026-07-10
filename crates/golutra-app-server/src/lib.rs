use std::{
    convert::Infallible,
    fs::{self, File, OpenOptions},
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
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
    ClientError, InProcessTransport, RUNTIME_DAEMON_ENV, RUNTIME_DAEMON_WORKSPACE_ENV,
    RuntimeClient, RuntimeHostInfo, event_sequence_no, runtime_endpoint_path,
};
use golutra_core::{SessionId, TaskId, ThreadId};
use golutra_protocol::{CommandAck, EventFilter, RuntimeQuery, SessionCommand};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct AppState {
    transport: InProcessTransport,
    info: RuntimeHostInfo,
}

impl AppState {
    #[must_use]
    pub fn new(transport: InProcessTransport) -> Self {
        let workspace_root = transport
            .workspace_root()
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        let info = runtime_host_info(&transport, &workspace_root, "http://127.0.0.1:0");
        Self { transport, info }
    }

    #[must_use]
    pub fn with_info(transport: InProcessTransport, info: RuntimeHostInfo) -> Self {
        Self { transport, info }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/runtime/info", get(runtime_info))
        .route("/attach", get(attach_page))
        .route("/commands", post(send_command))
        .route("/queries", post(query_runtime))
        .route("/events", get(events))
        .route("/events/replay", get(replay_events))
        .route("/threads", get(list_threads))
        .route("/threads/{thread_id}/resume", post(resume_thread))
        .route("/threads/{thread_id}/fork", post(fork_thread))
        .with_state(state)
        .layer(middleware::from_fn(enforce_local_http_boundary))
}

pub async fn run(addr: SocketAddr) -> miette::Result<()> {
    let workspace = std::env::current_dir().map_err(|error| miette::miette!("{error}"))?;
    run_workspace(addr, workspace).await
}

pub async fn run_workspace(
    addr: SocketAddr,
    workspace_root: impl AsRef<Path>,
) -> miette::Result<()> {
    validate_runtime_bind_addr(addr)?;
    let workspace_root = workspace_root
        .as_ref()
        .canonicalize()
        .map_err(|error| miette::miette!("{error}"))?;
    let lease = RuntimeDaemonLease::acquire(&workspace_root)?;
    let transport = InProcessTransport::for_workspace(&workspace_root)
        .await
        .map_err(|error| miette::miette!("{error}"))?;
    transport
        .recover_orphaned_tasks()
        .await
        .map_err(|error| miette::miette!("{error}"))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|error| miette::miette!("{error}"))?;
    let local_addr = listener
        .local_addr()
        .map_err(|error| miette::miette!("{error}"))?;
    let base_url = format!("http://{local_addr}");
    let info = runtime_host_info(&transport, &workspace_root, &base_url);
    lease.publish(&info)?;
    axum::serve(listener, router(AppState::with_info(transport, info)))
        .await
        .map_err(|error| miette::miette!("{error}"))
}

pub async fn run_embedded_daemon_if_requested() -> miette::Result<bool> {
    if std::env::var(RUNTIME_DAEMON_ENV).as_deref() != Ok("1") {
        return Ok(false);
    }
    let workspace = std::env::var_os(RUNTIME_DAEMON_WORKSPACE_ENV)
        .map(PathBuf::from)
        .ok_or_else(|| miette::miette!("{RUNTIME_DAEMON_WORKSPACE_ENV} is required"))?;
    run_workspace(SocketAddr::from(([127, 0, 0, 1], 0)), workspace).await?;
    Ok(true)
}

fn validate_runtime_bind_addr(addr: SocketAddr) -> miette::Result<()> {
    if addr.ip().is_loopback() {
        return Ok(());
    }
    Err(miette::miette!(
        "runtime app-server must bind to a loopback address until transport authentication is configured: {addr}"
    ))
}

fn prepare_runtime_lease_dir(workspace_root: &Path, runtime_dir: &Path) -> miette::Result<()> {
    match fs::symlink_metadata(runtime_dir) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(miette::miette!(
                "runtime directory cannot be a symbolic link: {}",
                runtime_dir.display()
            ));
        }
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(miette::miette!(
                "runtime path is not a directory: {}",
                runtime_dir.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(runtime_dir).map_err(|error| miette::miette!("{error}"))?;
        }
        Err(error) => return Err(miette::miette!("{error}")),
    }
    let canonical_runtime_dir = runtime_dir
        .canonicalize()
        .map_err(|error| miette::miette!("{error}"))?;
    if canonical_runtime_dir.parent() != Some(workspace_root) {
        return Err(miette::miette!(
            "runtime directory escaped the workspace: {}",
            canonical_runtime_dir.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(&canonical_runtime_dir, fs::Permissions::from_mode(0o700))
            .map_err(|error| miette::miette!("{error}"))?;
    }
    Ok(())
}

async fn enforce_local_http_boundary(request: Request, next: Next) -> Result<Response, StatusCode> {
    if local_http_headers(request.headers()) {
        Ok(next.run(request).await)
    } else {
        Err(StatusCode::FORBIDDEN)
    }
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

async fn runtime_info(State(state): State<AppState>) -> Json<RuntimeHostInfo> {
    Json(state.info)
}

async fn send_command(
    State(state): State<AppState>,
    Json(command): Json<SessionCommand>,
) -> Result<Json<CommandAck>, AppError> {
    Ok(Json(state.transport.send_command(command).await?))
}

async fn query_runtime(
    State(state): State<AppState>,
    Json(query): Json<RuntimeQuery>,
) -> Result<Json<Value>, AppError> {
    Ok(Json(state.transport.query(query).await?))
}

async fn attach_page() -> Html<&'static str> {
    Html(ATTACH_PAGE)
}

async fn events(
    State(state): State<AppState>,
    Query(query): Query<EventQuery>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, AppError> {
    let session_id = parse_session_id(&query.session_id)?;
    let task_id = query.task_id.as_deref().map(parse_task_id).transpose()?;
    let transport = state.transport.clone();
    let mut cursor = query.cursor;
    let filter = EventFilter {
        session_id,
        task_id,
        after_sequence_no: cursor,
    };
    let mut live = transport.subscribe_live(filter.clone());
    let replay = transport.replay_events(filter.clone()).await?;
    let stream = async_stream::stream! {
        for event in replay {
            cursor = event_sequence_no(&event).or(cursor);
            yield Ok::<Event, Infallible>(sse_event(event));
        }
        loop {
            match live.recv().await {
                Ok(event) => {
                    if event.session_id == session_id
                        && task_id.is_none_or(|task_id| event.task_id == Some(task_id))
                        && cursor.is_none_or(|cursor| event.sequence_no > cursor)
                    {
                        cursor = Some(event.sequence_no);
                        match serde_json::to_value(event) {
                            Ok(value) => yield Ok::<Event, Infallible>(sse_event(value)),
                            Err(error) => {
                                yield Ok::<Event, Infallible>(sse_named_event(
                                    "error",
                                    json!({"error": error.to_string()}),
                                ));
                            }
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    yield Ok::<Event, Infallible>(sse_named_event(
                        "lag",
                        json!({"skipped": skipped, "cursor": cursor}),
                    ));
                    match transport.replay_events(EventFilter {
                        session_id,
                        task_id,
                        after_sequence_no: cursor,
                    }).await {
                        Ok(events) => {
                            for event in events {
                                cursor = event_sequence_no(&event).or(cursor);
                                yield Ok::<Event, Infallible>(sse_event(event));
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
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    Ok(Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default()))
}

fn sse_event(value: Value) -> Event {
    let sequence_no = event_sequence_no(&value);
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
    Query(query): Query<EventQuery>,
) -> Result<Json<Vec<Value>>, AppError> {
    let filter = event_filter(query)?;
    Ok(Json(state.transport.replay_events(filter).await?))
}

#[derive(Debug, Deserialize)]
struct ThreadQuery {
    limit: Option<u32>,
}

async fn list_threads(
    State(state): State<AppState>,
    Query(query): Query<ThreadQuery>,
) -> Result<Json<Vec<golutra_store::ThreadRecord>>, AppError> {
    Ok(Json(
        state
            .transport
            .list_threads(query.limit.unwrap_or(20))
            .await?,
    ))
}

async fn resume_thread(
    State(state): State<AppState>,
    AxumPath(thread_id): AxumPath<String>,
) -> Result<Json<golutra_store::ThreadRecord>, AppError> {
    Ok(Json(
        state
            .transport
            .resume_thread(parse_thread_id(&thread_id)?)
            .await?,
    ))
}

async fn fork_thread(
    State(state): State<AppState>,
    AxumPath(thread_id): AxumPath<String>,
) -> Result<Json<golutra_store::ThreadRecord>, AppError> {
    Ok(Json(
        state
            .transport
            .fork_thread(parse_thread_id(&thread_id)?)
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
}

impl From<ClientError> for AppError {
    fn from(error: ClientError) -> Self {
        Self::Client(error)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match self {
            AppError::Client(error) => (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                error.to_string(),
            ),
            AppError::InvalidId(error) => (axum::http::StatusCode::BAD_REQUEST, error),
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

fn runtime_host_info(
    transport: &InProcessTransport,
    workspace_root: &Path,
    base_url: &str,
) -> RuntimeHostInfo {
    RuntimeHostInfo {
        instance_id: Uuid::now_v7().to_string(),
        pid: std::process::id(),
        base_url: base_url.to_owned(),
        workspace_root: workspace_root.display().to_string(),
        workspace_id: transport.workspace_id(),
        default_session_id: transport.default_session_id(),
        default_thread_id: transport.default_thread_id(),
        started_at: chrono::Utc::now(),
    }
}

struct RuntimeDaemonLease {
    _lock: File,
    endpoint_path: PathBuf,
}

impl RuntimeDaemonLease {
    fn acquire(workspace_root: &Path) -> miette::Result<Self> {
        let runtime_dir = workspace_root.join(".golutra");
        prepare_runtime_lease_dir(workspace_root, &runtime_dir)?;
        let lock_path = runtime_dir.join("runtime-host.lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|error| miette::miette!("{error}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            lock.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|error| miette::miette!("{error}"))?;
        }
        lock.try_lock_exclusive().map_err(|error| {
            miette::miette!(
                "workspace runtime host is already running for {}: {error}",
                workspace_root.display()
            )
        })?;
        Ok(Self {
            _lock: lock,
            endpoint_path: runtime_endpoint_path(workspace_root),
        })
    }

    fn publish(&self, info: &RuntimeHostInfo) -> miette::Result<()> {
        let parent = self
            .endpoint_path
            .parent()
            .ok_or_else(|| miette::miette!("runtime endpoint path has no parent"))?;
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temporary
                .as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|error| miette::miette!("{error}"))?;
        }
        temporary
            .persist(&self.endpoint_path)
            .map_err(|error| miette::miette!("{error}"))?;
        Ok(())
    }
}

impl Drop for RuntimeDaemonLease {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.endpoint_path);
    }
}

const ATTACH_PAGE: &str = r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>Golutra Attach</title>
    <style>
      :root {
        color-scheme: light dark;
        font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      }
      body {
        margin: 0;
        background: Canvas;
        color: CanvasText;
      }
      main {
        width: min(1120px, calc(100vw - 32px));
        margin: 24px auto;
        display: grid;
        gap: 16px;
      }
      form {
        display: grid;
        grid-template-columns: minmax(260px, 1fr) minmax(220px, 1fr) auto;
        gap: 8px;
        align-items: end;
      }
      label {
        display: grid;
        gap: 4px;
        font-size: 12px;
      }
      input, button, select {
        min-height: 36px;
        font: inherit;
      }
      button {
        padding: 0 14px;
      }
      section {
        display: grid;
        grid-template-columns: 1fr 1fr;
        gap: 16px;
      }
      pre {
        min-height: 320px;
        max-height: 70vh;
        overflow: auto;
        padding: 12px;
        border: 1px solid color-mix(in srgb, CanvasText 20%, transparent);
        border-radius: 6px;
        background: color-mix(in srgb, CanvasText 4%, transparent);
        white-space: pre-wrap;
        word-break: break-word;
      }
      @media (max-width: 760px) {
        form, section {
          grid-template-columns: 1fr;
        }
      }
    </style>
  </head>
  <body>
    <main>
      <form id="attach-form">
        <label>
          Session ID
          <input id="session-id" name="session_id" required autocomplete="off" />
        </label>
        <label>
          Task ID
          <input id="task-id" name="task_id" autocomplete="off" />
        </label>
        <label>
          Query
          <select id="query-kind" name="query_kind">
            <option value="user_projection">user_projection</option>
            <option value="debug_projection">debug_projection</option>
            <option value="session_state">session_state</option>
            <option value="task_state">task_state</option>
          </select>
        </label>
        <button type="submit">Attach</button>
      </form>
      <section>
        <pre id="projection" aria-live="polite"></pre>
        <pre id="events" aria-live="polite"></pre>
      </section>
    </main>
    <script>
      const form = document.getElementById("attach-form");
      const projection = document.getElementById("projection");
      const events = document.getElementById("events");
      let stream;

      function render(target, value) {
        target.textContent = typeof value === "string" ? value : JSON.stringify(value, null, 2);
      }

      form.addEventListener("submit", async (event) => {
        event.preventDefault();
        const sessionId = document.getElementById("session-id").value.trim();
        const taskId = document.getElementById("task-id").value.trim();
        const kind = document.getElementById("query-kind").value;
        const now = new Date().toISOString();

        if (stream) {
          stream.close();
        }

        const response = await fetch("/queries", {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({
            query_id: crypto.randomUUID(),
            session_id: sessionId,
            task_id: taskId || undefined,
            kind,
            requester: "web",
            timestamp: now
          })
        });
        render(projection, response.ok ? await response.json() : `query failed: ${response.status}`);

        const params = new URLSearchParams({ session_id: sessionId });
        if (taskId) {
          params.set("task_id", taskId);
        }
        events.textContent = "";
        stream = new EventSource(`/events?${params.toString()}`);
        stream.onmessage = (message) => {
          events.textContent += `${message.data}\n`;
        };
        stream.onerror = () => {
          events.textContent += "[event stream disconnected]\n";
          stream.close();
        };
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
    use golutra_client::HttpSseTransport;
    use golutra_core::{Actor, ActorKind, CommandId, QueryId, TaskStatus};
    use golutra_protocol::{RuntimeQuery, RuntimeQueryKind, SessionCommandKind};
    use tower::ServiceExt;

    use super::*;

    #[test]
    fn runtime_server_rejects_non_loopback_bind_addresses() {
        assert!(validate_runtime_bind_addr("127.0.0.1:0".parse().expect("IPv4")).is_ok());
        assert!(validate_runtime_bind_addr("[::1]:0".parse().expect("IPv6")).is_ok());

        let error = validate_runtime_bind_addr("0.0.0.0:47831".parse().expect("wildcard"))
            .expect_err("non-loopback bind must be rejected");

        assert!(
            error
                .to_string()
                .contains("must bind to a loopback address")
        );
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

        headers.remove(header::ORIGIN);
        headers.insert(header::HOST, "runtime.example:47831".parse().expect("host"));
        assert!(!local_http_headers(&headers));
    }

    #[cfg(unix)]
    #[test]
    fn runtime_lease_rejects_symlinked_runtime_directory() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        symlink(outside.path(), workspace.path().join(".golutra")).expect("symlink");

        let error = prepare_runtime_lease_dir(
            &workspace.path().canonicalize().expect("workspace path"),
            &workspace.path().join(".golutra"),
        )
        .expect_err("symlink must be rejected");

        assert!(error.to_string().contains("cannot be a symbolic link"));
    }

    #[tokio::test]
    async fn command_endpoint_accepts_session_command() {
        let transport = InProcessTransport::in_memory().await.expect("transport");
        let app = router(AppState::new(transport));
        let session_id = SessionId::new();
        let command = SessionCommand {
            command_id: CommandId::new(),
            session_id: Some(session_id),
            kind: SessionCommandKind::Prompt,
            idempotency_key: "http-test".to_owned(),
            actor: Actor {
                kind: ActorKind::Api,
                id: "test".to_owned(),
            },
            payload: json!({"prompt": "hello"}),
            timestamp: chrono::Utc::now(),
        };
        let request = Request::builder()
            .method("POST")
            .uri("/commands")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&command).expect("json")))
            .expect("request");

        let response = app.oneshot(request).await.expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let ack: CommandAck = serde_json::from_slice(&body).expect("ack");
        assert!(ack.accepted);
    }

    #[tokio::test]
    async fn attach_page_is_served() {
        let transport = InProcessTransport::in_memory().await.expect("transport");
        let app = router(AppState::new(transport));
        let request = Request::builder()
            .method("GET")
            .uri("/attach")
            .body(Body::empty())
            .expect("request");

        let response = app.oneshot(request).await.expect("response");

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn http_transport_receives_replay_and_live_sse_events() {
        let (transport, server) = http_transport().await;
        let session_id = transport.info().default_session_id;
        let mut events = transport
            .subscribe(EventFilter {
                session_id,
                task_id: None,
                after_sequence_no: None,
            })
            .await
            .expect("SSE subscription");

        let ack = transport
            .send_command(test_command(
                session_id,
                SessionCommandKind::Prompt,
                json!({"prompt": "hello"}),
            ))
            .await
            .expect("HTTP command");
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let event = events
                    .recv()
                    .await
                    .expect("stream remains open")
                    .expect("event");
                if event.event_type == golutra_protocol::RuntimeEventType::TaskCompleted {
                    return event;
                }
            }
        })
        .await
        .expect("terminal event arrives");
        let state = transport
            .query(RuntimeQuery {
                query_id: QueryId::new(),
                session_id,
                task_id: None,
                kind: RuntimeQueryKind::SessionState,
                requester: ActorKind::Sdk,
                cursor: None,
                timestamp: chrono::Utc::now(),
            })
            .await
            .expect("HTTP query");

        assert!(ack.accepted);
        assert_eq!(terminal.session_id, session_id);
        assert_eq!(state["task_status"], json!(TaskStatus::Completed));
        server.abort();
    }

    #[tokio::test]
    async fn http_transport_controls_approval_pause_pending_turn_and_abort() {
        let (transport, server) = http_transport().await;
        let session_id = transport.info().default_session_id;
        let mut events = transport
            .subscribe(EventFilter {
                session_id,
                task_id: None,
                after_sequence_no: None,
            })
            .await
            .expect("SSE subscription");
        transport
            .send_command(test_command(
                session_id,
                SessionCommandKind::Prompt,
                json!({"prompt": "sleep"}),
            ))
            .await
            .expect("start task");
        let approval_id = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let event = events
                    .recv()
                    .await
                    .expect("stream remains open")
                    .expect("event");
                if event.event_type == golutra_protocol::RuntimeEventType::ApprovalRequested {
                    return event.payload["approval_id"]
                        .as_str()
                        .expect("approval id")
                        .to_owned();
                }
            }
        })
        .await
        .expect("approval arrives");
        transport
            .send_command(test_command(
                session_id,
                SessionCommandKind::Approve,
                json!({"approval_id": approval_id}),
            ))
            .await
            .expect("approve");
        transport
            .send_command(test_command(
                session_id,
                SessionCommandKind::Pause,
                json!({}),
            ))
            .await
            .expect("pause");
        let queued = transport
            .send_command(test_command(
                session_id,
                SessionCommandKind::Prompt,
                json!({"prompt": "queued follow-up"}),
            ))
            .await
            .expect("queue turn");
        transport
            .send_command(test_command(
                session_id,
                SessionCommandKind::Abort,
                json!({}),
            ))
            .await
            .expect("abort");
        let aborted = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let event = events
                    .recv()
                    .await
                    .expect("stream remains open")
                    .expect("event");
                if event.event_type == golutra_protocol::RuntimeEventType::TaskAborted {
                    return event;
                }
            }
        })
        .await
        .expect("abort arrives");

        assert!(queued.accepted);
        assert_eq!(aborted.payload["status"], json!(TaskStatus::Cancelled));
        server.abort();
    }

    async fn http_transport() -> (HttpSseTransport, tokio::task::JoinHandle<()>) {
        let in_process = InProcessTransport::in_memory().await.expect("transport");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let workspace = std::env::current_dir().expect("workspace");
        let info = runtime_host_info(&in_process, &workspace, &format!("http://{address}"));
        let app = router(AppState::with_info(in_process, info));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("server");
        });
        let transport = HttpSseTransport::connect(format!("http://{address}"))
            .await
            .expect("HTTP transport");
        (transport, server)
    }

    fn test_command(
        session_id: SessionId,
        kind: SessionCommandKind,
        payload: Value,
    ) -> SessionCommand {
        SessionCommand {
            command_id: CommandId::new(),
            session_id: Some(session_id),
            kind,
            idempotency_key: CommandId::new().to_string(),
            actor: Actor {
                kind: ActorKind::Sdk,
                id: "http-test".to_owned(),
            },
            payload,
            timestamp: chrono::Utc::now(),
        }
    }
}
