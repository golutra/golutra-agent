use std::{convert::Infallible, net::SocketAddr, str::FromStr, time::Duration};

use axum::{
    Json, Router,
    extract::{Query, State},
    response::{
        Html, IntoResponse,
        sse::{Event, Sse},
    },
    routing::{get, post},
};
use golutra_client::{ClientError, InProcessTransport, RuntimeClient, event_sequence_no};
use golutra_core::{SessionId, TaskId};
use golutra_protocol::{CommandAck, EventFilter, RuntimeQuery, SessionCommand};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct AppState {
    transport: InProcessTransport,
}

impl AppState {
    #[must_use]
    pub fn new(transport: InProcessTransport) -> Self {
        Self { transport }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/attach", get(attach_page))
        .route("/commands", post(send_command))
        .route("/queries", post(query_runtime))
        .route("/events", get(events))
        .with_state(state)
}

pub async fn run(addr: SocketAddr) -> miette::Result<()> {
    let transport = InProcessTransport::for_current_workspace()
        .await
        .map_err(|error| miette::miette!("{error}"))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|error| miette::miette!("{error}"))?;
    axum::serve(listener, router(AppState::new(transport)))
        .await
        .map_err(|error| miette::miette!("{error}"))
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
    let stream = async_stream::stream! {
        loop {
            let filter = EventFilter {
                session_id,
                task_id,
                after_sequence_no: cursor,
            };
            match transport.replay_events(filter).await {
                Ok(events) => {
                    for event in events {
                        cursor = event_sequence_no(&event).or(cursor);
                        yield Ok::<Event, Infallible>(sse_event(event));
                    }
                }
                Err(error) => {
                    yield Ok::<Event, Infallible>(sse_event(json!({"error": error.to_string()})));
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    };
    Ok(Sse::new(stream))
}

fn sse_event(event: Value) -> Event {
    Event::default().json_data(event).unwrap_or_else(|_| {
        Event::default().data(json!({"error": "event serialization failed"}).to_string())
    })
}

#[derive(Debug, Deserialize)]
struct EventQuery {
    session_id: String,
    task_id: Option<String>,
    cursor: Option<u64>,
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
    use golutra_core::{Actor, ActorKind, CommandId};
    use golutra_protocol::SessionCommandKind;
    use tower::ServiceExt;

    use super::*;

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
}
