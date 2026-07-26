//! JSON-RPC adapters for the long-lived app server.
//!
//! Method handlers use the same EmbeddedTransport as the existing HTTP/SSE
//! routes.  The connection adapters below only deal with framing and event
//! notifications; they do not own runtime state.

use std::sync::Arc;

use axum::{
    Json,
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use golutra_client::{
    APP_SERVER_ATTACHMENT_HEADER, AgentClient, AgentEventProjector, RuntimeClient, RuntimeTransport,
};
use golutra_core::{Actor, ActorKind, CommandId, SessionId, ThreadId};
use golutra_protocol::{
    AgentThreadRef, AgentTurnStartResponse, EventFilter, EventPageDirection, EventPageRequest,
    JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, ProtocolVersionRange, RuntimeQuery,
    RuntimeQueryKind, SessionCommand, SessionCommandKind,
};
use serde_json::{Map, Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    sync::Mutex,
};
use uuid::Uuid;

use super::{AppError, AppState};

const MAX_RPC_REPLAY_EVENTS: u32 = 512;

pub async fn http_rpc(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: String,
) -> Response {
    let request = match serde_json::from_str::<JsonRpcRequest>(&body) {
        Ok(request) => request,
        Err(error) => {
            return Json(JsonRpcResponse::error(
                None,
                -32700,
                format!("invalid JSON-RPC request: {error}"),
            ))
            .into_response();
        }
    };
    let attachment = headers
        .get(APP_SERVER_ATTACHMENT_HEADER)
        .and_then(|value| value.to_str().ok());
    // The HTTP actor header is deliberately not authoritative. Once an
    // attachment exists, command handlers resolve the actor bound to that
    // server-issued attachment capability.
    let actor = connection_actor("http");
    match dispatch(&state, request, attachment, &actor).await {
        Some(response) => Json(response).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

pub async fn websocket_rpc(State(state): State<AppState>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| websocket_session(state, socket))
}

pub async fn serve_stdio(state: AppState) -> std::io::Result<()> {
    let reader = BufReader::new(tokio::io::stdin());
    let writer = Arc::new(Mutex::new(BufWriter::new(tokio::io::stdout())));
    let mut lines = reader.lines();
    let mut attachment_id = None;
    let actor = connection_actor("stdio");
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<JsonRpcRequest>(&line) {
            Ok(request) => dispatch(&state, request, attachment_id.as_deref(), &actor).await,
            Err(error) => Some(JsonRpcResponse::error(
                None,
                -32700,
                format!("invalid JSON-RPC request: {error}"),
            )),
        };
        let Some(response) = response else {
            continue;
        };
        if let Some(id) = response
            .result
            .as_ref()
            .and_then(|value| value.get("attachment_id"))
            .and_then(Value::as_str)
        {
            attachment_id = Some(id.to_owned());
        }
        write_json_line(&writer, &response).await?;
        if let Some((attachment, thread, command_id, cursor)) = turn_stream_spec(&response) {
            let state = state.clone();
            let writer = writer.clone();
            tokio::spawn(async move {
                let error_writer = writer.clone();
                if let Err(error) =
                    stream_stdio_events(&state, &attachment, thread, command_id, cursor, writer)
                        .await
                {
                    let _ = write_json_line(
                        &error_writer,
                        &JsonRpcNotification::new(
                            "agent/error",
                            json!({
                                "command_id": command_id,
                                "error": error.to_string(),
                            }),
                        ),
                    )
                    .await;
                }
            });
        }
    }
    Ok(())
}

async fn websocket_session(state: AppState, socket: WebSocket) {
    let (sink, mut source) = socket.split();
    let sink = Arc::new(Mutex::new(sink));
    let mut attachment_id = None;
    let actor = connection_actor("websocket");
    while let Some(Ok(message)) = source.next().await {
        let Message::Text(text) = message else {
            if matches!(message, Message::Close(_)) {
                break;
            }
            continue;
        };
        let response = match serde_json::from_str::<JsonRpcRequest>(&text) {
            Ok(request) => dispatch(&state, request, attachment_id.as_deref(), &actor).await,
            Err(error) => Some(JsonRpcResponse::error(
                None,
                -32700,
                format!("invalid JSON-RPC request: {error}"),
            )),
        };
        let Some(response) = response else {
            continue;
        };
        if let Some(id) = response
            .result
            .as_ref()
            .and_then(|value| value.get("attachment_id"))
            .and_then(Value::as_str)
        {
            attachment_id = Some(id.to_owned());
        }
        if send_ws_json(&sink, &response).await.is_err() {
            break;
        }
        if let Some((attachment, thread, command_id, cursor)) = turn_stream_spec(&response) {
            let state = state.clone();
            let sink = sink.clone();
            tokio::spawn(async move {
                let error_sink = sink.clone();
                if let Err(error) =
                    stream_ws_events(&state, &attachment, thread, command_id, cursor, sink).await
                {
                    let _ = send_ws_json(
                        &error_sink,
                        &JsonRpcNotification::new(
                            "agent/error",
                            json!({
                                "command_id": command_id,
                                "error": error,
                            }),
                        ),
                    )
                    .await;
                }
            });
        }
    }
}

async fn dispatch(
    state: &AppState,
    request: JsonRpcRequest,
    attachment_hint: Option<&str>,
    actor: &Actor,
) -> Option<JsonRpcResponse> {
    let id = request.id.clone();
    if request.jsonrpc != "2.0" {
        return id.map(|id| JsonRpcResponse::error(Some(id), -32600, "jsonrpc must be `2.0`"));
    }
    let params = request.params.unwrap_or_else(|| Value::Object(Map::new()));
    let result = match request.method.as_str() {
        "initialize" => Ok(json!({
            "server": "golutra-app-server",
            "protocol_versions": ProtocolVersionRange::runtime(),
            "capabilities": [
                "thread.start", "thread.resume", "thread.fork", "turn.start",
                "turn.steer", "turn.interrupt", "turn.takeover", "task.reconcile",
                "approval.resolve", "agent.event"
            ]
        })),
        "runtime/info" => Ok(json!(state.info())),
        "runtime/attach" => attach_from_params(state, &params).await,
        "thread/start" => thread_start(state, &params, attachment_hint, actor).await,
        "thread/resume" => thread_resume(state, &params, attachment_hint).await,
        "thread/fork" => thread_fork(state, &params, attachment_hint).await,
        "thread/list" => thread_list(state, &params, attachment_hint).await,
        "turn/start" => turn_start(state, &params, attachment_hint, actor).await,
        "turn/steer" => turn_control(state, &params, attachment_hint, actor, false).await,
        "turn/interrupt" => turn_control(state, &params, attachment_hint, actor, true).await,
        "turn/takeover" => turn_takeover(state, &params, attachment_hint, actor).await,
        "task/reconcile" => task_reconcile(state, &params, attachment_hint, actor).await,
        "approval/resolve" => approval_resolve(state, &params, attachment_hint, actor).await,
        "turn/status" => turn_status(state, &params, attachment_hint).await,
        "runtime/events/replay" => replay_events(state, &params, attachment_hint).await,
        _ => Err(RpcDispatchError::new(-32601, "method not found")),
    };
    let id = id?;
    Some(match result {
        Ok(value) => JsonRpcResponse::success(Some(id), value),
        Err(error) => JsonRpcResponse::error(Some(id), error.code, error.message),
    })
}

async fn attach_from_params(state: &AppState, params: &Value) -> RpcResult {
    let cwd = required_string(params, "cwd")?;
    let protocol_version = params
        .get("protocol_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcDispatchError::new(-32602, "protocol_version is required"))?;
    let protocol_version = u32::try_from(protocol_version)
        .map_err(|_| RpcDispatchError::new(-32602, "protocol_version is invalid"))?;
    if !ProtocolVersionRange::runtime().accepts(protocol_version) {
        return Err(RpcDispatchError::new(
            -32602,
            format!(
                "runtime protocol {} is incompatible with server range {}..{}",
                protocol_version,
                ProtocolVersionRange::runtime().minimum,
                ProtocolVersionRange::runtime().current
            ),
        ));
    }
    let attachment = state
        .attach_cwd(cwd)
        .await
        .map_err(RpcDispatchError::from_client)?;
    Ok(json!(attachment))
}

async fn thread_start(
    state: &AppState,
    params: &Value,
    attachment_hint: Option<&str>,
    _actor: &Actor,
) -> RpcResult {
    let (transport, attachment_id) = resolve_transport(state, params, attachment_hint).await?;
    let actor = state
        .attached_actor(&attachment_id)
        .await
        .map_err(RpcDispatchError::from_app)?;
    let client = AgentClient::with_actor(RuntimeTransport::Embedded(transport), actor);
    let thread = client
        .start_thread()
        .await
        .map_err(RpcDispatchError::from_client)?;
    Ok(json!({
        "attachment_id": attachment_id,
        "thread": thread.reference(),
    }))
}

async fn thread_resume(
    state: &AppState,
    params: &Value,
    attachment_hint: Option<&str>,
) -> RpcResult {
    let (transport, attachment_id) = resolve_transport(state, params, attachment_hint).await?;
    let thread_id = parse_thread(required_string(params, "thread_id")?)?;
    let record = transport
        .resume_thread(thread_id)
        .await
        .map_err(RpcDispatchError::from_client)?;
    Ok(json!({
        "attachment_id": attachment_id,
        "thread": AgentThreadRef {
            thread_id: record.thread_id,
            session_id: record.session_id,
            workspace_root: record.workspace_root,
        },
    }))
}

async fn thread_fork(state: &AppState, params: &Value, attachment_hint: Option<&str>) -> RpcResult {
    let (transport, attachment_id) = resolve_transport(state, params, attachment_hint).await?;
    let thread_id = parse_thread(required_string(params, "thread_id")?)?;
    let from_turn_id = params
        .get("from_turn_id")
        .and_then(Value::as_str)
        .map(|value| {
            value
                .parse()
                .map_err(|_| RpcDispatchError::new(-32602, "invalid turn_id"))
        })
        .transpose()?;
    let record = transport
        .fork_thread(thread_id, from_turn_id)
        .await
        .map_err(RpcDispatchError::from_client)?;
    Ok(json!({"attachment_id": attachment_id, "thread": record}))
}

async fn thread_list(state: &AppState, params: &Value, attachment_hint: Option<&str>) -> RpcResult {
    let (transport, attachment_id) = resolve_transport(state, params, attachment_hint).await?;
    let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(20) as u32;
    let threads = transport
        .list_threads(limit)
        .await
        .map_err(RpcDispatchError::from_client)?;
    Ok(json!({"attachment_id": attachment_id, "threads": threads}))
}

async fn turn_start(
    state: &AppState,
    params: &Value,
    attachment_hint: Option<&str>,
    _actor: &Actor,
) -> RpcResult {
    let (transport, attachment_id) = resolve_transport(state, params, attachment_hint).await?;
    let actor = state
        .attached_actor(&attachment_id)
        .await
        .map_err(RpcDispatchError::from_app)?;
    let thread = if let Some(value) = params.get("thread_id").and_then(Value::as_str) {
        let record = transport
            .resume_thread(parse_thread(value)?)
            .await
            .map_err(RpcDispatchError::from_client)?;
        AgentThreadRef {
            thread_id: record.thread_id,
            session_id: record.session_id,
            workspace_root: record.workspace_root,
        }
    } else {
        let client =
            AgentClient::with_actor(RuntimeTransport::Embedded(transport.clone()), actor.clone());
        client
            .start_thread()
            .await
            .map_err(RpcDispatchError::from_client)?
            .reference()
            .clone()
    };
    let prompt = required_string(params, "prompt")?;
    let cursor = session_cursor(&transport, thread.session_id).await?;
    let payload = json!({
        "prompt": prompt,
        "_thread_id": thread.thread_id,
        "output_schema": params.get("output_schema").cloned(),
        "allow_network": params.get("allow_network").cloned().unwrap_or_else(|| json!(false)),
        "completion_criteria": params.get("completion_criteria").cloned().unwrap_or_else(|| json!([])),
        "external_verifiers": params.get("external_verifiers").cloned().unwrap_or_else(|| json!([])),
    });
    let ack = transport
        .send_command(rpc_command(
            thread.session_id,
            SessionCommandKind::Prompt,
            payload,
            &actor,
        ))
        .await
        .map_err(RpcDispatchError::from_client)?;
    Ok(json!(AgentTurnStartResponse {
        attachment_id,
        thread,
        command_id: ack.command_id,
        accepted: ack.accepted,
        reason: ack.reason,
        cursor,
    }))
}

async fn turn_control(
    state: &AppState,
    params: &Value,
    attachment_hint: Option<&str>,
    _actor: &Actor,
    interrupt: bool,
) -> RpcResult {
    let (transport, attachment_id) = resolve_transport(state, params, attachment_hint).await?;
    let actor = state
        .attached_actor(&attachment_id)
        .await
        .map_err(RpcDispatchError::from_app)?;
    let session_id = resolve_session(&transport, params).await?;
    let (kind, payload) = if interrupt {
        (SessionCommandKind::Abort, json!({}))
    } else {
        (
            SessionCommandKind::Prompt,
            json!({
                "prompt": required_string(params, "prompt")?,
                "steer": true,
                "_thread_id": params.get("thread_id"),
            }),
        )
    };
    let ack = transport
        .send_command(rpc_command(session_id, kind, payload, &actor))
        .await
        .map_err(RpcDispatchError::from_client)?;
    Ok(json!({"attachment_id": attachment_id, "ack": ack}))
}

async fn turn_takeover(
    state: &AppState,
    params: &Value,
    attachment_hint: Option<&str>,
    _actor: &Actor,
) -> RpcResult {
    let (transport, attachment_id) = resolve_transport(state, params, attachment_hint).await?;
    let actor = state
        .attached_actor(&attachment_id)
        .await
        .map_err(RpcDispatchError::from_app)?;
    let session_id = resolve_session(&transport, params).await?;
    let ack = transport
        .send_command(rpc_command(
            session_id,
            SessionCommandKind::Takeover,
            json!({"_thread_id": params.get("thread_id")}),
            &actor,
        ))
        .await
        .map_err(RpcDispatchError::from_client)?;
    Ok(json!({"attachment_id": attachment_id, "ack": ack}))
}

async fn task_reconcile(
    state: &AppState,
    params: &Value,
    attachment_hint: Option<&str>,
    _actor: &Actor,
) -> RpcResult {
    let (transport, attachment_id) = resolve_transport(state, params, attachment_hint).await?;
    let actor = state
        .attached_actor(&attachment_id)
        .await
        .map_err(RpcDispatchError::from_app)?;
    let session_id = resolve_session(&transport, params).await?;
    let decision = params
        .get("decision")
        .cloned()
        .ok_or_else(|| RpcDispatchError::new(-32602, "decision is required"))?;
    let ack = transport
        .send_command(rpc_command(
            session_id,
            SessionCommandKind::ReconcileTask,
            json!({
                "task_id": params.get("task_id"),
                "decision": decision,
                "note": params.get("note"),
                "_thread_id": params.get("thread_id"),
            }),
            &actor,
        ))
        .await
        .map_err(RpcDispatchError::from_client)?;
    Ok(json!({"attachment_id": attachment_id, "ack": ack}))
}

async fn approval_resolve(
    state: &AppState,
    params: &Value,
    attachment_hint: Option<&str>,
    _actor: &Actor,
) -> RpcResult {
    let (transport, attachment_id) = resolve_transport(state, params, attachment_hint).await?;
    let actor = state
        .attached_actor(&attachment_id)
        .await
        .map_err(RpcDispatchError::from_app)?;
    let session_id = resolve_session(&transport, params).await?;
    let approve = params
        .get("approve")
        .and_then(Value::as_bool)
        .ok_or_else(|| RpcDispatchError::new(-32602, "approve must be a boolean"))?;
    let approval_id = required_string(params, "approval_id")?;
    let ack = transport
        .send_command(rpc_command(
            session_id,
            if approve {
                SessionCommandKind::Approve
            } else {
                SessionCommandKind::Deny
            },
            json!({
                "approval_id": approval_id,
                "_thread_id": params.get("thread_id"),
            }),
            &actor,
        ))
        .await
        .map_err(RpcDispatchError::from_client)?;
    Ok(json!({"attachment_id": attachment_id, "ack": ack}))
}

async fn turn_status(state: &AppState, params: &Value, attachment_hint: Option<&str>) -> RpcResult {
    let (transport, attachment_id) = resolve_transport(state, params, attachment_hint).await?;
    let session_id = resolve_session(&transport, params).await?;
    let value = transport
        .query(RuntimeQuery {
            query_id: golutra_core::QueryId::new(),
            session_id,
            task_id: None,
            kind: RuntimeQueryKind::SessionState,
            requester: ActorKind::Api,
            cursor: None,
            timestamp: chrono::Utc::now(),
        })
        .await
        .map_err(RpcDispatchError::from_client)?;
    Ok(json!({"attachment_id": attachment_id, "state": value}))
}

async fn replay_events(
    state: &AppState,
    params: &Value,
    attachment_hint: Option<&str>,
) -> RpcResult {
    let (transport, attachment_id) = resolve_transport(state, params, attachment_hint).await?;
    let session_id = resolve_session(&transport, params).await?;
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(128)
        .clamp(1, u64::from(MAX_RPC_REPLAY_EVENTS)) as u32;
    let page = transport
        .event_page(EventPageRequest {
            session_id,
            task_id: None,
            cursor: params.get("cursor").and_then(Value::as_u64),
            direction: EventPageDirection::Forward,
            limit,
        })
        .await
        .map_err(RpcDispatchError::from_client)?;
    Ok(json!({
        "attachment_id": attachment_id,
        "events": page.events,
        "start_cursor": page.start_cursor,
        "end_cursor": page.end_cursor,
        "has_more": page.has_more,
        "next_cursor": page.end_cursor,
        "limit": limit,
    }))
}

async fn resolve_transport(
    state: &AppState,
    params: &Value,
    attachment_hint: Option<&str>,
) -> Result<(golutra_client::EmbeddedTransport, String), RpcDispatchError> {
    if let Some(attachment_id) = params
        .get("attachment_id")
        .and_then(Value::as_str)
        .or(attachment_hint)
    {
        return state
            .attached_transport_id(attachment_id)
            .await
            .map(|transport| (transport, attachment_id.to_owned()))
            .map_err(RpcDispatchError::from_app);
    }
    let cwd = params
        .get("cwd")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcDispatchError::new(-32602, "attachment_id or cwd is required"))?;
    let attachment = state
        .attach_cwd(cwd)
        .await
        .map_err(RpcDispatchError::from_client)?;
    Ok((
        state
            .attached_transport_id(&attachment.attachment_id)
            .await
            .map_err(RpcDispatchError::from_app)?,
        attachment.attachment_id,
    ))
}

async fn session_cursor(
    transport: &golutra_client::EmbeddedTransport,
    session_id: SessionId,
) -> Result<Option<u64>, RpcDispatchError> {
    let value = transport
        .query(RuntimeQuery {
            query_id: golutra_core::QueryId::new(),
            session_id,
            task_id: None,
            kind: RuntimeQueryKind::SessionState,
            requester: ActorKind::Api,
            cursor: None,
            timestamp: chrono::Utc::now(),
        })
        .await
        .map_err(RpcDispatchError::from_client)?;
    Ok(value.get("last_sequence_no").and_then(Value::as_u64))
}

async fn resolve_session(
    transport: &golutra_client::EmbeddedTransport,
    params: &Value,
) -> Result<SessionId, RpcDispatchError> {
    if let Some(value) = params.get("session_id").and_then(Value::as_str) {
        return value
            .parse()
            .map_err(|_| RpcDispatchError::new(-32602, "invalid session_id"));
    }
    if let Some(value) = params.get("thread_id").and_then(Value::as_str) {
        let record = transport
            .resume_thread(parse_thread(value)?)
            .await
            .map_err(RpcDispatchError::from_client)?;
        return Ok(record.session_id);
    }
    Ok(transport.default_session_id())
}

fn turn_stream_spec(
    response: &JsonRpcResponse,
) -> Option<(String, AgentThreadRef, CommandId, Option<u64>)> {
    let result = response.result.as_ref()?;
    if result.get("accepted").and_then(Value::as_bool) != Some(true)
        || result.get("cursor").is_none()
    {
        return None;
    }
    let attachment = result.get("attachment_id")?.as_str()?.to_owned();
    let thread: AgentThreadRef = serde_json::from_value(result.get("thread")?.clone()).ok()?;
    let command_id = result
        .get("command_id")
        .and_then(Value::as_str)?
        .parse()
        .ok()?;
    Some((
        attachment,
        thread,
        command_id,
        result.get("cursor").and_then(Value::as_u64),
    ))
}

async fn stream_stdio_events(
    state: &AppState,
    attachment: &str,
    thread: AgentThreadRef,
    command_id: CommandId,
    cursor: Option<u64>,
    writer: Arc<Mutex<BufWriter<tokio::io::Stdout>>>,
) -> std::io::Result<()> {
    let transport = state
        .attached_transport_id(attachment)
        .await
        .map_err(|error| std::io::Error::other(format!("{error:?}")))?;
    let mut stream = transport
        .subscribe(EventFilter {
            session_id: thread.session_id,
            task_id: None,
            after_sequence_no: cursor,
        })
        .await
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let mut projector = AgentEventProjector::new(thread, Some(command_id));
    write_json_line(
        &writer,
        &JsonRpcNotification::new("agent/event", json!({"event": projector.thread_started()})),
    )
    .await?;
    while let Some(event) = stream.recv().await {
        let event = event.map_err(|error| std::io::Error::other(error.to_string()))?;
        let Some(event) = projector.project(event) else {
            continue;
        };
        let terminal = projector.is_finished();
        let notification = JsonRpcNotification::new("agent/event", json!({"event": event}));
        write_json_line(&writer, &notification).await?;
        if terminal {
            return Ok(());
        }
    }
    Err(std::io::Error::other(
        "agent event stream ended before turn completion",
    ))
}

async fn stream_ws_events(
    state: &AppState,
    attachment: &str,
    thread: AgentThreadRef,
    command_id: CommandId,
    cursor: Option<u64>,
    sink: Arc<Mutex<futures_util::stream::SplitSink<WebSocket, Message>>>,
) -> Result<(), String> {
    let transport = state
        .attached_transport_id(attachment)
        .await
        .map_err(|error| format!("{error:?}"))?;
    let mut stream = transport
        .subscribe(EventFilter {
            session_id: thread.session_id,
            task_id: None,
            after_sequence_no: cursor,
        })
        .await
        .map_err(|error| format!("{error}"))?;
    let mut projector = AgentEventProjector::new(thread, Some(command_id));
    send_ws_json(
        &sink,
        &JsonRpcNotification::new("agent/event", json!({"event": projector.thread_started()})),
    )
    .await
    .map_err(|error| format!("{error}"))?;
    while let Some(event) = stream.recv().await {
        let event = event.map_err(|error| format!("{error}"))?;
        let Some(event) = projector.project(event) else {
            continue;
        };
        let terminal = projector.is_finished();
        let notification = JsonRpcNotification::new("agent/event", json!({"event": event}));
        send_ws_json(&sink, &notification)
            .await
            .map_err(|error| format!("{error}"))?;
        if terminal {
            return Ok(());
        }
    }
    Err("agent event stream ended before turn completion".to_owned())
}

async fn write_json_line<T: serde::Serialize>(
    writer: &Arc<Mutex<BufWriter<tokio::io::Stdout>>>,
    value: &T,
) -> std::io::Result<()> {
    let mut writer = writer.lock().await;
    let line = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    writer.write_all(&line).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
}

async fn send_ws_json<T: serde::Serialize>(
    sink: &Arc<Mutex<futures_util::stream::SplitSink<WebSocket, Message>>>,
    value: &T,
) -> Result<(), axum::Error> {
    let text = serde_json::to_string(value).map_err(axum::Error::new)?;
    sink.lock().await.send(Message::Text(text.into())).await
}

fn required_string<'a>(params: &'a Value, name: &str) -> Result<&'a str, RpcDispatchError> {
    params
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| RpcDispatchError::new(-32602, format!("{name} is required")))
}

fn parse_thread(value: &str) -> Result<ThreadId, RpcDispatchError> {
    value
        .parse()
        .map_err(|_| RpcDispatchError::new(-32602, "invalid thread_id"))
}

fn rpc_command(
    session_id: SessionId,
    kind: SessionCommandKind,
    payload: Value,
    actor: &Actor,
) -> SessionCommand {
    SessionCommand {
        command_id: CommandId::new(),
        session_id: Some(session_id),
        kind,
        idempotency_key: format!("rpc-{}", CommandId::new()),
        actor: actor.clone(),
        payload,
        timestamp: chrono::Utc::now(),
    }
}

fn connection_actor(transport: &str) -> Actor {
    Actor {
        kind: ActorKind::Api,
        id: format!("{transport}-{}", Uuid::now_v7()),
    }
}

type RpcResult = Result<Value, RpcDispatchError>;

#[derive(Debug)]
struct RpcDispatchError {
    code: i32,
    message: String,
}

impl RpcDispatchError {
    fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn from_client(error: golutra_client::ClientError) -> Self {
        Self::new(-32000, error.to_string())
    }

    fn from_app(error: AppError) -> Self {
        Self::new(-32001, format!("{error:?}"))
    }
}
