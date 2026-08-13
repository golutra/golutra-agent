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
    APP_SERVER_ATTACHMENT_HEADER, AgentClient, AgentEventProjector, RuntimeOperation,
    RuntimeOperationClient, RuntimeTransport,
};
use golutra_core::{Actor, ActorKind, CommandId, SessionId, ThreadId};
use golutra_protocol::{
    AgentThreadRef, AgentTurnStartResponse, EventFilter, EventPageDirection, EventPageRequest,
    JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, MAX_WIRE_MESSAGE_BYTES,
    ProtocolVersionRange, RuntimeQuery, RuntimeQueryKind, SessionCommand, SessionCommandKind,
};
use serde_json::{Map, Value, json};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    sync::Mutex,
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
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
    let mut temporary_attachment = TemporaryAttachmentGuard::new(state.clone());
    let response = dispatch(
        &state,
        request,
        attachment,
        &actor,
        temporary_attachment.slot(),
    )
    .await;
    temporary_attachment
        .release_unretained(response.as_ref())
        .await;
    match response {
        Some(response) => Json(response).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

pub async fn websocket_rpc(State(state): State<AppState>, upgrade: WebSocketUpgrade) -> Response {
    upgrade
        .max_message_size(MAX_WIRE_MESSAGE_BYTES)
        .max_frame_size(MAX_WIRE_MESSAGE_BYTES)
        .on_upgrade(move |socket| websocket_session(state, socket))
}

pub async fn serve_stdio(state: AppState) -> std::io::Result<()> {
    let reader = BufReader::new(tokio::io::stdin());
    let writer = Arc::new(Mutex::new(BufWriter::new(tokio::io::stdout())));
    let mut reader = reader;
    let mut line = Vec::new();
    let mut attachment_id = None;
    let mut stream_tasks = Vec::new();
    let actor = connection_actor("stdio");
    loop {
        reap_finished_stream_tasks(&mut stream_tasks).await;
        let read = read_bounded_line(&mut reader, &mut line, MAX_WIRE_MESSAGE_BYTES).await?;
        if read == 0 {
            break;
        }
        if !line.ends_with(b"\n") || line.len().saturating_sub(1) > MAX_WIRE_MESSAGE_BYTES {
            write_json_line(
                &writer,
                &JsonRpcResponse::error(
                    None,
                    -32700,
                    format!(
                        "stdio JSON-RPC request exceeds {MAX_WIRE_MESSAGE_BYTES} bytes or its framing limit"
                    ),
                ),
            )
            .await?;
            break;
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let mut temporary_attachment = TemporaryAttachmentGuard::new(state.clone());
        let mut detach_notification = false;
        let (method, response) = match serde_json::from_slice::<JsonRpcRequest>(&line) {
            Ok(request) => {
                let method = request.method.clone();
                detach_notification =
                    notification_detaches_current(&request, attachment_id.as_deref());
                (
                    Some(method),
                    dispatch(
                        &state,
                        request,
                        attachment_id.as_deref(),
                        &actor,
                        temporary_attachment.slot(),
                    )
                    .await,
                )
            }
            Err(error) => (
                None,
                Some(JsonRpcResponse::error(
                    None,
                    -32700,
                    format!("invalid JSON-RPC request: {error}"),
                )),
            ),
        };
        let Some(response) = response else {
            temporary_attachment.release_unretained(None).await;
            if detach_notification {
                cancel_stream_tasks(std::mem::take(&mut stream_tasks)).await;
                attachment_id = None;
            }
            continue;
        };
        if let Some(method) = method {
            update_connection_attachment(
                &state,
                &mut attachment_id,
                &method,
                &response,
                &mut stream_tasks,
            )
            .await;
        }
        temporary_attachment
            .release_unretained(Some(&response))
            .await;
        write_json_line(&writer, &response).await?;
        if let Some((attachment, thread, command_id, cursor)) = turn_stream_spec(&response) {
            let stream_transport = match state.attached_transport_id(&attachment).await {
                Ok(transport) => transport,
                Err(error) => {
                    write_json_line(
                        &writer,
                        &JsonRpcNotification::new(
                            "agent/error",
                            json!({
                                "command_id": command_id,
                                "error": format!("{error:?}"),
                            }),
                        ),
                    )
                    .await?;
                    continue;
                }
            };
            let writer = writer.clone();
            let cancellation = stream_transport.cancellation();
            stream_tasks.push(tokio::spawn(async move {
                let error_writer = writer.clone();
                if let Err(error) =
                    stream_stdio_events(stream_transport, thread, command_id, cursor, writer).await
                {
                    let _ = write_json_line_cancellable(
                        &error_writer,
                        &JsonRpcNotification::new(
                            "agent/error",
                            json!({
                                "command_id": command_id,
                                "error": format!("{error:?}"),
                            }),
                        ),
                        &cancellation,
                    )
                    .await;
                }
            }));
        }
    }
    cancel_stream_tasks(stream_tasks).await;
    if let Some(attachment_id) = attachment_id {
        state.detach_attachment(&attachment_id).await;
    }
    Ok(())
}

async fn websocket_session(state: AppState, socket: WebSocket) {
    let (sink, mut source) = socket.split();
    let sink = Arc::new(Mutex::new(sink));
    let mut attachment_id = None;
    let mut stream_tasks = Vec::new();
    let actor = connection_actor("websocket");
    while let Some(Ok(message)) = source.next().await {
        reap_finished_stream_tasks(&mut stream_tasks).await;
        let Message::Text(text) = message else {
            if matches!(message, Message::Close(_)) {
                break;
            }
            continue;
        };
        let mut temporary_attachment = TemporaryAttachmentGuard::new(state.clone());
        let mut detach_notification = false;
        let (method, response) = match serde_json::from_str::<JsonRpcRequest>(&text) {
            Ok(request) => {
                let method = request.method.clone();
                detach_notification =
                    notification_detaches_current(&request, attachment_id.as_deref());
                (
                    Some(method),
                    dispatch(
                        &state,
                        request,
                        attachment_id.as_deref(),
                        &actor,
                        temporary_attachment.slot(),
                    )
                    .await,
                )
            }
            Err(error) => (
                None,
                Some(JsonRpcResponse::error(
                    None,
                    -32700,
                    format!("invalid JSON-RPC request: {error}"),
                )),
            ),
        };
        let Some(response) = response else {
            temporary_attachment.release_unretained(None).await;
            if detach_notification {
                cancel_stream_tasks(std::mem::take(&mut stream_tasks)).await;
                attachment_id = None;
            }
            continue;
        };
        if let Some(method) = method {
            update_connection_attachment(
                &state,
                &mut attachment_id,
                &method,
                &response,
                &mut stream_tasks,
            )
            .await;
        }
        temporary_attachment
            .release_unretained(Some(&response))
            .await;
        if send_ws_json(&sink, &response).await.is_err() {
            break;
        }
        if let Some((attachment, thread, command_id, cursor)) = turn_stream_spec(&response) {
            let stream_transport = match state.attached_transport_id(&attachment).await {
                Ok(transport) => transport,
                Err(error) => {
                    let _ = send_ws_json(
                        &sink,
                        &JsonRpcNotification::new(
                            "agent/error",
                            json!({
                                "command_id": command_id,
                                "error": format!("{error:?}"),
                            }),
                        ),
                    )
                    .await;
                    continue;
                }
            };
            let sink = sink.clone();
            let cancellation = stream_transport.cancellation();
            stream_tasks.push(tokio::spawn(async move {
                let error_sink = sink.clone();
                if let Err(error) =
                    stream_ws_events(stream_transport, thread, command_id, cursor, sink).await
                {
                    let _ = send_ws_json_cancellable(
                        &error_sink,
                        &JsonRpcNotification::new(
                            "agent/error",
                            json!({
                                "command_id": command_id,
                                "error": error,
                            }),
                        ),
                        &cancellation,
                    )
                    .await;
                }
            }));
        }
    }
    cancel_stream_tasks(stream_tasks).await;
    if let Some(attachment_id) = attachment_id {
        state.detach_attachment(&attachment_id).await;
    }
}

async fn dispatch(
    state: &AppState,
    request: JsonRpcRequest,
    attachment_hint: Option<&str>,
    actor: &Actor,
    temporary_attachment: &mut Option<String>,
) -> Option<JsonRpcResponse> {
    let id = request.id.clone();
    if request.jsonrpc != "2.0" {
        return id.map(|id| JsonRpcResponse::error(Some(id), -32600, "jsonrpc must be `2.0`"));
    }
    // A turn notification has no response envelope from which the connection
    // adapters could derive the event-stream subscription. Ignore it rather
    // than starting work that is invisible to the caller; clients must use a
    // request id when they need the streamed turn lifecycle.
    if id.is_none() && matches!(request.method.as_str(), "runtime/attach" | "turn/start") {
        return None;
    }
    let params = request.params.unwrap_or_else(|| Value::Object(Map::new()));
    let result = match request.method.as_str() {
        "initialize" => Ok(json!({
            "server": "golutra-app-server",
            "protocol_versions": ProtocolVersionRange::runtime(),
            "capabilities": [
                "thread.start", "thread.resume", "thread.fork", "turn.start",
                "turn.steer", "turn.interrupt", "turn.takeover", "task.reconcile",
                "approval.resolve", "agent.event", "runtime.detach"
            ]
        })),
        "runtime/info" => Ok(json!(state.info())),
        "runtime/attach" => attach_from_params(state, &params).await,
        "runtime/detach" => detach_from_params(state, &params, attachment_hint).await,
        "thread/start" => {
            thread_start(state, &params, attachment_hint, actor, temporary_attachment).await
        }
        "thread/resume" => {
            thread_resume(state, &params, attachment_hint, temporary_attachment).await
        }
        "thread/fork" => thread_fork(state, &params, attachment_hint, temporary_attachment).await,
        "thread/list" => thread_list(state, &params, attachment_hint, temporary_attachment).await,
        "turn/start" => {
            turn_start(state, &params, attachment_hint, actor, temporary_attachment).await
        }
        "turn/steer" => {
            turn_control(
                state,
                &params,
                attachment_hint,
                actor,
                false,
                temporary_attachment,
            )
            .await
        }
        "turn/interrupt" => {
            turn_control(
                state,
                &params,
                attachment_hint,
                actor,
                true,
                temporary_attachment,
            )
            .await
        }
        "turn/takeover" => {
            turn_takeover(state, &params, attachment_hint, actor, temporary_attachment).await
        }
        "task/reconcile" => {
            task_reconcile(state, &params, attachment_hint, actor, temporary_attachment).await
        }
        "approval/resolve" => {
            approval_resolve(state, &params, attachment_hint, actor, temporary_attachment).await
        }
        "turn/status" => turn_status(state, &params, attachment_hint, temporary_attachment).await,
        "runtime/events/replay" => {
            replay_events(state, &params, attachment_hint, temporary_attachment).await
        }
        _ => Err(RpcDispatchError::new(-32601, "method not found")),
    };
    let id = id?;
    Some(match result {
        Ok(value) => JsonRpcResponse::success(Some(id), value),
        Err(error) => JsonRpcResponse::error(Some(id), error.code, error.message),
    })
}

/// Own a cwd-derived attachment for the entire lifetime of one RPC request.
///
/// A request future can be cancelled at any await point, including after
/// `attach_cwd` succeeds and before the response is handed to the connection
/// adapter. `Drop` schedules the revoke in that case; the normal path
/// disarms only after the adapter has decided whether the response retains the
/// attachment.
struct TemporaryAttachmentGuard {
    state: AppState,
    attachment_id: Option<String>,
}

impl TemporaryAttachmentGuard {
    fn new(state: AppState) -> Self {
        Self {
            state,
            attachment_id: None,
        }
    }

    fn slot(&mut self) -> &mut Option<String> {
        &mut self.attachment_id
    }

    async fn release_unretained(&mut self, response: Option<&JsonRpcResponse>) {
        let Some(attachment_id) = self.attachment_id.take() else {
            return;
        };
        let returned_attachment = response
            .and_then(|response| response.result.as_ref())
            .and_then(|result| result.get("attachment_id"))
            .and_then(Value::as_str);
        if returned_attachment != Some(attachment_id.as_str()) {
            self.state.detach_attachment(&attachment_id).await;
        }
    }
}

impl Drop for TemporaryAttachmentGuard {
    fn drop(&mut self) {
        let Some(attachment_id) = self.attachment_id.take() else {
            return;
        };
        let state = self.state.clone();
        // All request futures are polled inside Tokio. `try_current` keeps the
        // guard harmless if an owner is dropped during runtime construction,
        // while the normal cancellation path gets an asynchronous revoke.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                state.detach_attachment(&attachment_id).await;
            });
        }
    }
}

#[cfg(test)]
async fn release_unretained_temporary_attachment(
    state: &AppState,
    temporary_attachment: &mut Option<String>,
    response: Option<&JsonRpcResponse>,
) {
    let Some(attachment_id) = temporary_attachment.take() else {
        return;
    };
    let returned_attachment = response
        .and_then(|response| response.result.as_ref())
        .and_then(|result| result.get("attachment_id"))
        .and_then(Value::as_str);
    if returned_attachment != Some(attachment_id.as_str()) {
        state.detach_attachment(&attachment_id).await;
    }
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

async fn detach_from_params(
    state: &AppState,
    params: &Value,
    attachment_hint: Option<&str>,
) -> RpcResult {
    let attachment_id = resolve_requested_attachment(params, attachment_hint)?
        .ok_or_else(|| RpcDispatchError::new(-32602, "attachment_id is required"))?;
    if !state.detach_attachment(attachment_id).await {
        return Err(RpcDispatchError::new(
            -32001,
            "runtime attachment was not found",
        ));
    }
    Ok(json!({"attachment_id": attachment_id, "detached": true}))
}

async fn update_connection_attachment(
    state: &AppState,
    current: &mut Option<String>,
    method: &str,
    response: &JsonRpcResponse,
    stream_tasks: &mut Vec<JoinHandle<()>>,
) {
    let Some(result) = response.result.as_ref() else {
        return;
    };
    let Some(attachment_id) = result.get("attachment_id").and_then(Value::as_str) else {
        return;
    };
    if method == "runtime/detach" {
        if current.as_deref() == Some(attachment_id) {
            cancel_stream_tasks(std::mem::take(stream_tasks)).await;
            *current = None;
        }
        return;
    }
    if current.as_deref().is_some_and(|old| old != attachment_id)
        && let Some(old) = current.take()
    {
        // A stream task owns a clone of the old transport. Releasing the
        // registry entry alone does not stop it from publishing notifications
        // into this connection, so cancel those tasks before rotating the
        // connection-bound capability.
        cancel_stream_tasks(std::mem::take(stream_tasks)).await;
        state.detach_attachment(&old).await;
    }
    *current = Some(attachment_id.to_owned());
}

async fn thread_start(
    state: &AppState,
    params: &Value,
    attachment_hint: Option<&str>,
    _actor: &Actor,
    temporary_attachment: &mut Option<String>,
) -> RpcResult {
    let (transport, attachment_id) =
        resolve_transport(state, params, attachment_hint, temporary_attachment).await?;
    let actor = transport.actor.clone();
    let client = AgentClient::with_actor(
        RuntimeTransport::Embedded(transport.transport.clone()),
        actor,
    );
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
    temporary_attachment: &mut Option<String>,
) -> RpcResult {
    let (transport, attachment_id) =
        resolve_transport(state, params, attachment_hint, temporary_attachment).await?;
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

async fn thread_fork(
    state: &AppState,
    params: &Value,
    attachment_hint: Option<&str>,
    temporary_attachment: &mut Option<String>,
) -> RpcResult {
    let (transport, attachment_id) =
        resolve_transport(state, params, attachment_hint, temporary_attachment).await?;
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

async fn thread_list(
    state: &AppState,
    params: &Value,
    attachment_hint: Option<&str>,
    temporary_attachment: &mut Option<String>,
) -> RpcResult {
    let (transport, attachment_id) =
        resolve_transport(state, params, attachment_hint, temporary_attachment).await?;
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
    temporary_attachment: &mut Option<String>,
) -> RpcResult {
    let (transport, attachment_id) =
        resolve_transport(state, params, attachment_hint, temporary_attachment).await?;
    let actor = transport.actor.clone();
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
        let client = AgentClient::with_actor(
            RuntimeTransport::Embedded(transport.transport.clone()),
            actor.clone(),
        );
        client
            .start_thread()
            .await
            .map_err(RpcDispatchError::from_client)?
            .reference()
            .clone()
    };
    let prompt = required_string(params, "prompt")?;
    let cursor = session_cursor(&transport, thread.session_id).await?;
    let payload = turn_payload_from_params(params, thread.thread_id, prompt);
    let ack = transport
        .execute_operation(RuntimeOperation::SendCommand(rpc_command(
            thread.session_id,
            SessionCommandKind::Prompt,
            payload,
            &actor,
        )))
        .await
        .map_err(RpcDispatchError::from_client)?
        .into_command_ack()
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

fn turn_payload_from_params(params: &Value, thread_id: ThreadId, prompt: &str) -> Value {
    let mut payload = json!({
        "prompt": prompt,
        "_thread_id": thread_id,
        "task_contract": params.get("task_contract").cloned().unwrap_or(Value::Null),
        "output_schema": params.get("output_schema").cloned(),
        "completion_criteria": params.get("completion_criteria").cloned().unwrap_or_else(|| json!([])),
    });
    if let Some(execution_mode) = params.get("execution_mode") {
        payload["execution_mode"] = execution_mode.clone();
    }
    if let Some(tool_profile) = params.get("tool_profile") {
        payload["tool_profile"] = tool_profile.clone();
    }
    if let Some(allow_network) = params.get("allow_network") {
        payload["allow_network"] = allow_network.clone();
    }
    if let Some(yolo) = params.get("yolo") {
        payload["yolo"] = yolo.clone();
    }
    if let Some(max_elapsed_ms) = params.get("max_elapsed_ms") {
        payload["max_elapsed_ms"] = max_elapsed_ms.clone();
    }
    if let Some(defer_external_verification) = params.get("defer_external_verification") {
        payload["defer_external_verification"] = defer_external_verification.clone();
    }
    if let Some(discover_project_verifiers) = params.get("discover_project_verifiers") {
        payload["discover_project_verifiers"] = discover_project_verifiers.clone();
    }
    if let Some(external_verifiers) = params.get("external_verifiers") {
        payload["external_verifiers"] = external_verifiers.clone();
    } else if params
        .get("discover_project_verifiers")
        .and_then(Value::as_bool)
        == Some(false)
    {
        payload["external_verifiers"] = json!([]);
    }
    payload
}

fn turn_steer_payload_from_params(params: &Value) -> Result<Value, RpcDispatchError> {
    for field in [
        "execution_mode",
        "task_contract",
        "output_schema",
        "completion_criteria",
        "external_verifiers",
        "max_elapsed_ms",
        "defer_external_verification",
    ] {
        if params.get(field).is_some() {
            return Err(RpcDispatchError::new(
                -32602,
                format!("turn/steer cannot change {field}"),
            ));
        }
    }

    let mut payload = json!({
        "prompt": required_string(params, "prompt")?,
        "steer": true,
        "_thread_id": params.get("thread_id"),
    });
    if let Some(tool_profile) = params.get("tool_profile") {
        payload["tool_profile"] = tool_profile.clone();
    }
    Ok(payload)
}

async fn turn_control(
    state: &AppState,
    params: &Value,
    attachment_hint: Option<&str>,
    _actor: &Actor,
    interrupt: bool,
    temporary_attachment: &mut Option<String>,
) -> RpcResult {
    let (kind, payload) = if interrupt {
        (SessionCommandKind::Abort, json!({}))
    } else {
        (
            SessionCommandKind::Prompt,
            turn_steer_payload_from_params(params)?,
        )
    };
    let (transport, attachment_id) =
        resolve_transport(state, params, attachment_hint, temporary_attachment).await?;
    let actor = transport.actor.clone();
    let session_id = resolve_session(&transport, params).await?;
    let ack = transport
        .execute_operation(RuntimeOperation::SendCommand(rpc_command(
            session_id, kind, payload, &actor,
        )))
        .await
        .map_err(RpcDispatchError::from_client)?
        .into_command_ack()
        .map_err(RpcDispatchError::from_client)?;
    Ok(json!({"attachment_id": attachment_id, "ack": ack}))
}

async fn turn_takeover(
    state: &AppState,
    params: &Value,
    attachment_hint: Option<&str>,
    _actor: &Actor,
    temporary_attachment: &mut Option<String>,
) -> RpcResult {
    let (transport, attachment_id) =
        resolve_transport(state, params, attachment_hint, temporary_attachment).await?;
    let actor = transport.actor.clone();
    let session_id = resolve_session(&transport, params).await?;
    let ack = transport
        .execute_operation(RuntimeOperation::SendCommand(rpc_command(
            session_id,
            SessionCommandKind::Takeover,
            json!({"_thread_id": params.get("thread_id")}),
            &actor,
        )))
        .await
        .map_err(RpcDispatchError::from_client)?
        .into_command_ack()
        .map_err(RpcDispatchError::from_client)?;
    Ok(json!({"attachment_id": attachment_id, "ack": ack}))
}

async fn task_reconcile(
    state: &AppState,
    params: &Value,
    attachment_hint: Option<&str>,
    _actor: &Actor,
    temporary_attachment: &mut Option<String>,
) -> RpcResult {
    let (transport, attachment_id) =
        resolve_transport(state, params, attachment_hint, temporary_attachment).await?;
    let actor = transport.actor.clone();
    let session_id = resolve_session(&transport, params).await?;
    let decision = params
        .get("decision")
        .cloned()
        .ok_or_else(|| RpcDispatchError::new(-32602, "decision is required"))?;
    let ack = transport
        .execute_operation(RuntimeOperation::SendCommand(rpc_command(
            session_id,
            SessionCommandKind::ReconcileTask,
            json!({
                "task_id": params.get("task_id"),
                "decision": decision,
                "note": params.get("note"),
                "_thread_id": params.get("thread_id"),
            }),
            &actor,
        )))
        .await
        .map_err(RpcDispatchError::from_client)?
        .into_command_ack()
        .map_err(RpcDispatchError::from_client)?;
    Ok(json!({"attachment_id": attachment_id, "ack": ack}))
}

async fn approval_resolve(
    state: &AppState,
    params: &Value,
    attachment_hint: Option<&str>,
    _actor: &Actor,
    temporary_attachment: &mut Option<String>,
) -> RpcResult {
    let (transport, attachment_id) =
        resolve_transport(state, params, attachment_hint, temporary_attachment).await?;
    let actor = transport.actor.clone();
    let session_id = resolve_session(&transport, params).await?;
    let approve = params
        .get("approve")
        .and_then(Value::as_bool)
        .ok_or_else(|| RpcDispatchError::new(-32602, "approve must be a boolean"))?;
    let approval_id = required_string(params, "approval_id")?;
    let ack = transport
        .execute_operation(RuntimeOperation::SendCommand(rpc_command(
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
        )))
        .await
        .map_err(RpcDispatchError::from_client)?
        .into_command_ack()
        .map_err(RpcDispatchError::from_client)?;
    Ok(json!({"attachment_id": attachment_id, "ack": ack}))
}

async fn turn_status(
    state: &AppState,
    params: &Value,
    attachment_hint: Option<&str>,
    temporary_attachment: &mut Option<String>,
) -> RpcResult {
    let (transport, attachment_id) =
        resolve_transport(state, params, attachment_hint, temporary_attachment).await?;
    let session_id = resolve_session(&transport, params).await?;
    let value = transport
        .execute_operation(RuntimeOperation::Query(RuntimeQuery {
            query_id: golutra_core::QueryId::new(),
            session_id,
            task_id: None,
            kind: RuntimeQueryKind::SessionState,
            requester: ActorKind::Api,
            cursor: None,
            timestamp: chrono::Utc::now(),
        }))
        .await
        .map_err(RpcDispatchError::from_client)?
        .into_query()
        .map_err(RpcDispatchError::from_client)?;
    Ok(json!({"attachment_id": attachment_id, "state": value}))
}

async fn replay_events(
    state: &AppState,
    params: &Value,
    attachment_hint: Option<&str>,
    temporary_attachment: &mut Option<String>,
) -> RpcResult {
    let (transport, attachment_id) =
        resolve_transport(state, params, attachment_hint, temporary_attachment).await?;
    let session_id = resolve_session(&transport, params).await?;
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(128)
        .clamp(1, u64::from(MAX_RPC_REPLAY_EVENTS)) as u32;
    let page = transport
        .execute_operation(RuntimeOperation::EventPage(EventPageRequest {
            session_id,
            task_id: None,
            cursor: params.get("cursor").and_then(Value::as_u64),
            direction: EventPageDirection::Forward,
            limit,
        }))
        .await
        .map_err(RpcDispatchError::from_client)?
        .into_event_page()
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
    temporary_attachment: &mut Option<String>,
) -> Result<(crate::attachment_registry::AttachedAttachment, String), RpcDispatchError> {
    if let Some(attachment_id) = resolve_requested_attachment(params, attachment_hint)? {
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
    let attachment_id = attachment.attachment_id;
    *temporary_attachment = Some(attachment_id.clone());
    let transport = state
        .attached_transport_id(&attachment_id)
        .await
        .map_err(RpcDispatchError::from_app)?;
    Ok((transport, attachment_id))
}

fn resolve_requested_attachment<'a>(
    params: &'a Value,
    attachment_hint: Option<&'a str>,
) -> Result<Option<&'a str>, RpcDispatchError> {
    let requested = params.get("attachment_id").and_then(Value::as_str);
    if let (Some(bound), Some(requested)) = (attachment_hint, requested)
        && bound != requested
    {
        return Err(RpcDispatchError::new(
            -32001,
            "attachment_id does not match the connection-bound attachment",
        ));
    }
    Ok(attachment_hint.or(requested))
}

fn notification_detaches_current(request: &JsonRpcRequest, current: Option<&str>) -> bool {
    if request.id.is_some() || request.method != "runtime/detach" {
        return false;
    }
    let Some(current) = current else {
        return false;
    };
    request
        .params
        .as_ref()
        .and_then(|params| params.get("attachment_id"))
        .and_then(Value::as_str)
        .is_none_or(|requested| requested == current)
}

async fn session_cursor(
    transport: &golutra_client::EmbeddedTransport,
    session_id: SessionId,
) -> Result<Option<u64>, RpcDispatchError> {
    let value = transport
        .execute_operation(RuntimeOperation::Query(RuntimeQuery {
            query_id: golutra_core::QueryId::new(),
            session_id,
            task_id: None,
            kind: RuntimeQueryKind::SessionState,
            requester: ActorKind::Api,
            cursor: None,
            timestamp: chrono::Utc::now(),
        }))
        .await
        .map_err(RpcDispatchError::from_client)?
        .into_query()
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
    transport: crate::attachment_registry::AttachedAttachment,
    thread: AgentThreadRef,
    command_id: CommandId,
    cursor: Option<u64>,
    writer: Arc<Mutex<BufWriter<tokio::io::Stdout>>>,
) -> std::io::Result<()> {
    let cancellation = transport.cancellation();
    let subscription = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Ok(()),
        result = transport.execute_operation(RuntimeOperation::Subscribe(EventFilter {
            session_id: thread.session_id,
            task_id: None,
            after_sequence_no: cursor,
        })) => result,
    };
    let mut stream = subscription
        .map_err(|error| std::io::Error::other(error.to_string()))?
        .into_subscription()
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let mut projector = AgentEventProjector::new(thread, Some(command_id));
    if !write_json_line_cancellable(
        &writer,
        &JsonRpcNotification::new("agent/event", json!({"event": projector.thread_started()})),
        &cancellation,
    )
    .await?
    {
        return Ok(());
    }
    loop {
        let event = tokio::select! {
            _ = cancellation.cancelled() => return Ok(()),
            event = stream.recv() => event,
        };
        let Some(event) = event else {
            break;
        };
        let event = event.map_err(|error| std::io::Error::other(error.to_string()))?;
        let Some(event) = projector.project(event) else {
            continue;
        };
        let terminal = projector.is_finished();
        let notification = JsonRpcNotification::new("agent/event", json!({"event": event}));
        if !write_json_line_cancellable(&writer, &notification, &cancellation).await? {
            return Ok(());
        }
        if terminal {
            return Ok(());
        }
    }
    Err(std::io::Error::other(
        "agent event stream ended before turn completion",
    ))
}

async fn stream_ws_events(
    transport: crate::attachment_registry::AttachedAttachment,
    thread: AgentThreadRef,
    command_id: CommandId,
    cursor: Option<u64>,
    sink: Arc<Mutex<futures_util::stream::SplitSink<WebSocket, Message>>>,
) -> Result<(), String> {
    let cancellation = transport.cancellation();
    let subscription = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Ok(()),
        result = transport.execute_operation(RuntimeOperation::Subscribe(EventFilter {
            session_id: thread.session_id,
            task_id: None,
            after_sequence_no: cursor,
        })) => result,
    };
    let mut stream = subscription
        .map_err(|error| format!("{error}"))?
        .into_subscription()
        .map_err(|error| format!("{error}"))?;
    let mut projector = AgentEventProjector::new(thread, Some(command_id));
    if !send_ws_json_cancellable(
        &sink,
        &JsonRpcNotification::new("agent/event", json!({"event": projector.thread_started()})),
        &cancellation,
    )
    .await
    .map_err(|error| format!("{error}"))?
    {
        return Ok(());
    }
    loop {
        let event = tokio::select! {
            _ = cancellation.cancelled() => return Ok(()),
            event = stream.recv() => event,
        };
        let Some(event) = event else {
            break;
        };
        let event = event.map_err(|error| format!("{error}"))?;
        let Some(event) = projector.project(event) else {
            continue;
        };
        let terminal = projector.is_finished();
        let notification = JsonRpcNotification::new("agent/event", json!({"event": event}));
        if !send_ws_json_cancellable(&sink, &notification, &cancellation)
            .await
            .map_err(|error| format!("{error}"))?
        {
            return Ok(());
        }
        if terminal {
            return Ok(());
        }
    }
    Err("agent event stream ended before turn completion".to_owned())
}

async fn cancel_stream_tasks(tasks: Vec<JoinHandle<()>>) {
    for task in tasks {
        task.abort();
        let _ = task.await;
    }
}

async fn reap_finished_stream_tasks(tasks: &mut Vec<JoinHandle<()>>) {
    let mut active = Vec::with_capacity(tasks.len());
    for task in tasks.drain(..) {
        if task.is_finished() {
            let _ = task.await;
        } else {
            active.push(task);
        }
    }
    *tasks = active;
}

async fn write_json_line<T: serde::Serialize>(
    writer: &Arc<Mutex<BufWriter<tokio::io::Stdout>>>,
    value: &T,
) -> std::io::Result<()> {
    let mut writer = writer.lock().await;
    let line = bounded_json_bytes(value)?;
    writer.write_all(&line).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
}

async fn write_json_line_cancellable<T: serde::Serialize>(
    writer: &Arc<Mutex<BufWriter<tokio::io::Stdout>>>,
    value: &T,
    cancellation: &CancellationToken,
) -> std::io::Result<bool> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Ok(false),
        result = write_json_line(writer, value) => result.map(|()| true),
    }
}

async fn send_ws_json<T: serde::Serialize>(
    sink: &Arc<Mutex<futures_util::stream::SplitSink<WebSocket, Message>>>,
    value: &T,
) -> Result<(), axum::Error> {
    let text = bounded_json_string(value).map_err(axum::Error::new)?;
    sink.lock().await.send(Message::Text(text.into())).await
}

fn bounded_json_bytes<T: serde::Serialize>(value: &T) -> std::io::Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    if bytes.len().saturating_add(1) > MAX_WIRE_MESSAGE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("outgoing JSON-RPC frame exceeds {MAX_WIRE_MESSAGE_BYTES} bytes"),
        ));
    }
    Ok(bytes)
}

fn bounded_json_string<T: serde::Serialize>(value: &T) -> Result<String, std::io::Error> {
    let bytes = bounded_json_bytes(value)?;
    String::from_utf8(bytes).map_err(std::io::Error::other)
}

async fn send_ws_json_cancellable<T: serde::Serialize>(
    sink: &Arc<Mutex<futures_util::stream::SplitSink<WebSocket, Message>>>,
    value: &T,
    cancellation: &CancellationToken,
) -> Result<bool, axum::Error> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Ok(false),
        result = send_ws_json(sink, value) => result.map(|()| true),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state() -> AppState {
        AppState::new(
            golutra_client::AppServerInfo {
                instance_id: Uuid::now_v7().to_string(),
                pid: std::process::id(),
                base_url: "http://127.0.0.1:0".to_owned(),
                ipc_path: None,
                protocol_versions: ProtocolVersionRange::runtime(),
                started_at: chrono::Utc::now(),
            },
            &"t".repeat(64),
        )
        .expect("app state")
    }

    async fn insert_test_attachment(state: &AppState, attachment_id: &str) {
        state
            .inner
            .attachments
            .lock()
            .await
            .insert(
                attachment_id.to_owned(),
                golutra_client::EmbeddedTransport::in_memory()
                    .await
                    .expect("transport"),
                connection_actor("test-attachment"),
                std::path::PathBuf::from("/test-workspace"),
                std::time::Instant::now(),
            )
            .expect("attachment insert");
    }

    #[tokio::test]
    async fn runtime_detach_rpc_revokes_the_attachment() {
        let state = test_state();
        let attachment_id = Uuid::now_v7().to_string();
        insert_test_attachment(&state, &attachment_id).await;
        let request = serde_json::from_value(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "runtime/detach",
            "params": {"attachment_id": attachment_id},
        }))
        .expect("request");

        let mut temporary_attachment = None;
        let response = dispatch(
            &state,
            request,
            None,
            &connection_actor("test"),
            &mut temporary_attachment,
        )
        .await
        .expect("response");

        assert_eq!(
            response
                .result
                .as_ref()
                .and_then(|result| result.get("detached")),
            Some(&Value::Bool(true))
        );
        assert!(state.attached_transport_id(&attachment_id).await.is_err());

        let repeated = serde_json::from_value(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "runtime/detach",
            "params": {"attachment_id": attachment_id},
        }))
        .expect("request");
        let response = dispatch(
            &state,
            repeated,
            None,
            &connection_actor("test"),
            &mut temporary_attachment,
        )
        .await
        .expect("response");
        assert_eq!(
            response.error.as_ref().map(|error| error.code),
            Some(-32001)
        );
    }

    #[tokio::test]
    async fn runtime_detach_notification_revokes_without_returning_a_response() {
        let state = test_state();
        let attachment_id = Uuid::now_v7().to_string();
        insert_test_attachment(&state, &attachment_id).await;
        let request = serde_json::from_value(json!({
            "jsonrpc": "2.0",
            "method": "runtime/detach",
            "params": {"attachment_id": attachment_id},
        }))
        .expect("request");

        let mut temporary_attachment = None;
        let response = dispatch(
            &state,
            request,
            None,
            &connection_actor("test"),
            &mut temporary_attachment,
        )
        .await;

        assert!(response.is_none());
        assert!(state.attached_transport_id(&attachment_id).await.is_err());
    }

    #[tokio::test]
    async fn turn_start_notification_is_ignored_without_running_a_turn() {
        let state = test_state();
        let request = serde_json::from_value(json!({
            "jsonrpc": "2.0",
            "method": "turn/start",
            "params": {"prompt": "this must not be started"},
        }))
        .expect("request");
        let mut temporary_attachment = None;

        let response = dispatch(
            &state,
            request,
            None,
            &connection_actor("test"),
            &mut temporary_attachment,
        )
        .await;

        assert!(response.is_none());
        assert!(temporary_attachment.is_none());
    }

    #[tokio::test]
    async fn connection_reattach_releases_its_previous_capability() {
        let state = test_state();
        let previous = Uuid::now_v7().to_string();
        let replacement = Uuid::now_v7().to_string();
        insert_test_attachment(&state, &previous).await;
        insert_test_attachment(&state, &replacement).await;
        let mut current = Some(previous.clone());
        let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
        let (stopped_sender, stopped_receiver) = tokio::sync::oneshot::channel();
        struct StopSignal(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for StopSignal {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }
        let task = tokio::spawn(async move {
            let _signal = StopSignal(Some(stopped_sender));
            let _ = started_sender.send(());
            std::future::pending::<()>().await;
        });
        started_receiver.await.expect("stream task started");
        let mut stream_tasks = vec![task];
        let response =
            JsonRpcResponse::success(Some(json!(1)), json!({"attachment_id": replacement}));

        update_connection_attachment(
            &state,
            &mut current,
            "runtime/attach",
            &response,
            &mut stream_tasks,
        )
        .await;

        tokio::time::timeout(std::time::Duration::from_secs(1), stopped_receiver)
            .await
            .expect("stream task cancellation")
            .expect("stream task stop signal");
        assert!(stream_tasks.is_empty());
        assert_eq!(current.as_deref(), Some(replacement.as_str()));
        assert!(state.attached_transport_id(&previous).await.is_err());
        assert!(state.attached_transport_id(&replacement).await.is_ok());
    }

    #[tokio::test]
    async fn connection_bound_rpc_cannot_select_a_foreign_attachment() {
        let state = test_state();
        let bound = Uuid::now_v7().to_string();
        let foreign = Uuid::now_v7().to_string();
        insert_test_attachment(&state, &bound).await;
        insert_test_attachment(&state, &foreign).await;
        let request = serde_json::from_value(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "thread/list",
            "params": {"attachment_id": foreign},
        }))
        .expect("request");

        let mut temporary_attachment = None;
        let response = dispatch(
            &state,
            request,
            Some(&bound),
            &connection_actor("test"),
            &mut temporary_attachment,
        )
        .await
        .expect("response");

        assert_eq!(
            response.error.as_ref().map(|error| error.code),
            Some(-32001)
        );
        assert!(state.attached_transport_id(&bound).await.is_ok());
        assert!(state.attached_transport_id(&foreign).await.is_ok());
    }

    #[tokio::test]
    async fn failed_rpc_with_cwd_releases_its_temporary_attachment() {
        let state = test_state();
        let workspace = tempfile::tempdir().expect("workspace");

        for id in 1..=3 {
            let request = serde_json::from_value(json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "thread/resume",
                "params": {
                    "cwd": workspace.path(),
                    "thread_id": "not-a-thread-id",
                },
            }))
            .expect("request");
            let mut temporary_attachment = None;

            let response = dispatch(
                &state,
                request,
                None,
                &connection_actor("test"),
                &mut temporary_attachment,
            )
            .await
            .expect("error response");
            assert_eq!(
                response.error.as_ref().map(|error| error.code),
                Some(-32602)
            );

            release_unretained_temporary_attachment(
                &state,
                &mut temporary_attachment,
                Some(&response),
            )
            .await;
            assert_eq!(state.inner.attachments.lock().await.len(), 0);
        }
    }

    #[tokio::test]
    async fn notification_with_cwd_releases_its_temporary_attachment() {
        let state = test_state();
        let workspace = tempfile::tempdir().expect("workspace");

        for _ in 0..3 {
            let request = serde_json::from_value(json!({
                "jsonrpc": "2.0",
                "method": "thread/list",
                "params": {"cwd": workspace.path()},
            }))
            .expect("request");
            let mut temporary_attachment = None;

            let response = dispatch(
                &state,
                request,
                None,
                &connection_actor("test"),
                &mut temporary_attachment,
            )
            .await;
            assert!(response.is_none());

            release_unretained_temporary_attachment(&state, &mut temporary_attachment, None).await;
            assert_eq!(state.inner.attachments.lock().await.len(), 0);
        }
    }

    #[tokio::test]
    async fn temporary_attachment_guard_revokes_when_request_future_is_dropped() {
        let state = test_state();
        let attachment_id = Uuid::now_v7().to_string();
        insert_test_attachment(&state, &attachment_id).await;
        let mut guard = TemporaryAttachmentGuard::new(state.clone());
        *guard.slot() = Some(attachment_id.clone());
        drop(guard);

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if state.attached_transport_id(&attachment_id).await.is_err() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("drop cleanup must detach the attachment");
    }

    #[tokio::test]
    async fn a_transport_clone_survives_connection_attachment_cleanup() {
        let state = test_state();
        let attachment_id = Uuid::now_v7().to_string();
        insert_test_attachment(&state, &attachment_id).await;
        let attachment = state
            .attached_transport_id(&attachment_id)
            .await
            .expect("attached transport");
        let transport = attachment.transport.clone();
        drop(attachment);
        assert!(state.detach_attachment(&attachment_id).await);

        let threads = transport.list_threads(10).await.expect("cloned transport");
        assert!(threads.is_empty());
        assert!(state.attached_transport_id(&attachment_id).await.is_err());
    }

    #[test]
    fn turn_payload_forwards_an_empty_verifier_list_when_discovery_is_disabled() {
        let params = json!({
            "discover_project_verifiers": false,
            "completion_criteria": ["tests pass"],
        });

        let payload = turn_payload_from_params(&params, ThreadId::new(), "fix the tests");

        assert!(payload.get("execution_mode").is_none());
        assert!(payload.get("tool_profile").is_none());
        assert_eq!(payload["discover_project_verifiers"], false);
        assert_eq!(payload["external_verifiers"], json!([]));
    }

    #[tokio::test]
    async fn finished_stream_handles_are_reaped_without_touching_active_streams() {
        let finished = tokio::spawn(async {});
        let active = tokio::spawn(async {
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;

        let mut tasks = vec![finished, active];
        reap_finished_stream_tasks(&mut tasks).await;

        assert_eq!(tasks.len(), 1);
        assert!(!tasks[0].is_finished());
        tasks.pop().expect("active stream handle").abort();
    }

    #[test]
    fn turn_payload_forwards_malformed_discovery_flag_for_central_validation() {
        let payload = turn_payload_from_params(
            &json!({"discover_project_verifiers": "false"}),
            ThreadId::new(),
            "fix the tests",
        );

        assert_eq!(payload["discover_project_verifiers"], "false");
        assert!(payload.get("external_verifiers").is_none());
    }

    #[test]
    fn turn_payload_preserves_omitted_execution_options_for_legacy_clients() {
        let default_payload =
            turn_payload_from_params(&json!({}), ThreadId::new(), "fix the tests");

        assert!(default_payload.get("execution_mode").is_none());
        assert!(default_payload.get("tool_profile").is_none());
    }

    #[test]
    fn turn_payload_forwards_explicit_execution_options() {
        let payload = turn_payload_from_params(
            &json!({
                "execution_mode": "strict",
                "tool_profile": "full",
            }),
            ThreadId::new(),
            "run the checked task",
        );

        assert_eq!(payload["execution_mode"], "strict");
        assert_eq!(payload["tool_profile"], "full");
    }

    #[test]
    fn turn_payload_preserves_default_discovery_and_explicit_verifiers() {
        let default_payload =
            turn_payload_from_params(&json!({}), ThreadId::new(), "fix the tests");
        assert!(default_payload.get("external_verifiers").is_none());
        assert!(default_payload.get("allow_network").is_none());

        let explicit = json!([{"program": "cargo", "args": ["test"]}]);
        let explicit_payload = turn_payload_from_params(
            &json!({
                "discover_project_verifiers": false,
                "external_verifiers": explicit,
            }),
            ThreadId::new(),
            "fix the tests",
        );
        assert_eq!(explicit_payload["external_verifiers"], explicit);

        let network_payload = turn_payload_from_params(
            &json!({"allow_network": true}),
            ThreadId::new(),
            "fetch dependencies",
        );
        assert_eq!(network_payload["allow_network"], true);

        let yolo_payload = turn_payload_from_params(
            &json!({"yolo": true}),
            ThreadId::new(),
            "modify an external path",
        );
        assert_eq!(yolo_payload["yolo"], true);

        let bounded_payload = turn_payload_from_params(
            &json!({
                "max_elapsed_ms": 345_000,
                "defer_external_verification": true,
            }),
            ThreadId::new(),
            "finish before the harness deadline",
        );
        assert_eq!(bounded_payload["max_elapsed_ms"], 345_000);
        assert_eq!(bounded_payload["defer_external_verification"], true);
    }

    #[tokio::test]
    async fn turn_steer_rpc_rejects_active_task_semantic_overrides() {
        let state = test_state();
        let forbidden_fields = [
            ("execution_mode", json!("strict")),
            (
                "task_contract",
                json!({"require_objective_validation": true}),
            ),
            ("output_schema", json!({"type": "object"})),
            ("completion_criteria", json!(["tests pass"])),
            ("external_verifiers", json!([])),
            ("max_elapsed_ms", json!(30_000)),
            ("defer_external_verification", json!(false)),
        ];

        for (index, (field, value)) in forbidden_fields.into_iter().enumerate() {
            let mut params = json!({"prompt": "continue the active task"});
            params[field] = value;
            let request = serde_json::from_value(json!({
                "jsonrpc": "2.0",
                "id": index,
                "method": "turn/steer",
                "params": params,
            }))
            .expect("request");
            let mut temporary_attachment = None;

            let response = dispatch(
                &state,
                request,
                None,
                &connection_actor("test"),
                &mut temporary_attachment,
            )
            .await
            .expect("error response");

            assert_eq!(
                response.error.as_ref().map(|error| error.code),
                Some(-32602),
                "{field}"
            );
            assert!(
                response
                    .error
                    .as_ref()
                    .is_some_and(|error| error.message.contains(field)),
                "{field}"
            );
        }
    }

    #[test]
    fn turn_steer_payload_forwards_explicit_tool_profile() {
        let payload = turn_steer_payload_from_params(&json!({
            "thread_id": "thread-1",
            "prompt": "continue the active task",
            "tool_profile": "full",
        }))
        .expect("steer payload");

        assert_eq!(payload["prompt"], "continue the active task");
        assert_eq!(payload["steer"], true);
        assert_eq!(payload["_thread_id"], "thread-1");
        assert_eq!(payload["tool_profile"], "full");
    }

    #[tokio::test]
    async fn stdio_line_reader_bounds_input_before_json_parsing() {
        let mut input = vec![b'x'; MAX_WIRE_MESSAGE_BYTES + 32];
        input.push(b'\n');
        let mut reader = BufReader::new(std::io::Cursor::new(input));
        let mut line = Vec::new();

        let read = read_bounded_line(&mut reader, &mut line, MAX_WIRE_MESSAGE_BYTES)
            .await
            .expect("bounded line");

        assert!(read <= MAX_WIRE_MESSAGE_BYTES + 1);
        assert!(!line.ends_with(b"\n"));
        assert_eq!(line.len(), MAX_WIRE_MESSAGE_BYTES + 1);
    }

    #[tokio::test]
    async fn stdio_stream_write_stops_when_its_attachment_is_revoked() {
        let writer = Arc::new(Mutex::new(BufWriter::new(tokio::io::stdout())));
        let _writer_guard = writer.lock().await;
        let cancellation = CancellationToken::new();
        let value = JsonRpcNotification::new("agent/event", json!({"event": "blocked"}));
        let task = tokio::spawn({
            let writer = writer.clone();
            let cancellation = cancellation.clone();
            async move { write_json_line_cancellable(&writer, &value, &cancellation).await }
        });
        tokio::task::yield_now().await;
        cancellation.cancel();

        assert!(matches!(task.await.expect("write task"), Ok(false)));
    }

    #[test]
    fn outgoing_json_frames_share_the_wire_limit() {
        let small = bounded_json_bytes(&json!({"ok": true})).expect("small frame");
        assert!(small.len() < MAX_WIRE_MESSAGE_BYTES);

        let oversized = json!({"payload": "x".repeat(MAX_WIRE_MESSAGE_BYTES) });
        let error = bounded_json_bytes(&oversized).expect_err("oversized frame");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("outgoing JSON-RPC frame"));
    }
}
