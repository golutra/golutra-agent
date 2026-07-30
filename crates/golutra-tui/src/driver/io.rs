use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use fs2::FileExt;
use golutra_client::RuntimeTransport;
use golutra_protocol::{
    DriverEnvelope, DriverNotification, DriverNotificationKind, DriverRequest, DriverResponse,
    DriverResponseEnvelope, RowRange, SnapshotDetail, SnapshotPanes, SnapshotRequest,
    SnapshotScope, TuiFrame, WaitCondition, response,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::mpsc,
};
use uuid::Uuid;

use super::{
    DEFAULT_WAIT_MILLIS, MAX_DRIVER_LINE_BYTES, MAX_PENDING_WAITS, MAX_WAIT_MILLIS, TuiDriver,
    bounded_error, driver_error_code,
};
use crate::{Args, DriverArgs, InspectArgs};

const DRIVER_SYNC_TIMEOUT: Duration = Duration::from_secs(1);

pub(crate) async fn run_inspect_command(args: &Args, command: InspectArgs) -> miette::Result<()> {
    let cwd = driver_cwd(args)?;
    let (scope, panes) = parse_view(&command.view)?;
    let detail = parse_detail(&command.detail)?;
    let transport = driver_transport(args, command.embedded, &cwd).await?;
    let mut driver = TuiDriver::launch(
        transport,
        command.session.as_deref().or(args.session_id.as_deref()),
        args.task_id.as_deref(),
        args.debug,
        args.yolo,
        command.width,
        command.height,
    )
    .await?;

    let wait = command
        .wait
        .as_deref()
        .unwrap_or(if command.prompt.is_some() {
            "auto"
        } else {
            "none"
        });
    if let Some(prompt) = command.prompt {
        match driver
            .try_handle(DriverRequest::InputPrompt { text: prompt })
            .await?
        {
            DriverResponse::Accepted { .. } => {}
            response => return Err(unexpected_response("submit prompt", response)),
        }
    }
    if wait != "none" {
        let condition = inspect_wait_condition(wait, panes)?;
        match driver
            .try_handle(DriverRequest::Wait {
                until: condition,
                timeout_ms: Some(command.timeout_ms),
            })
            .await?
        {
            DriverResponse::WaitResult { .. } => {}
            DriverResponse::WaitTimeout { .. } => {
                return Err(miette::miette!(
                    "wait_timeout: condition {wait} was not reached within {}ms",
                    command.timeout_ms
                ));
            }
            response => return Err(unexpected_response("wait", response)),
        }
    }

    let frame = driver
        .snapshot(SnapshotRequest {
            scope,
            panes,
            width: command.width,
            height: command.height,
            rows: command.rows.as_deref().map(parse_row_range).transpose()?,
            frame_id: None,
            detail,
        })
        .await?;
    write_inspect_output(&command.format, frame).await
}

fn inspect_wait_condition(value: &str, panes: SnapshotPanes) -> miette::Result<WaitCondition> {
    if value.trim().eq_ignore_ascii_case("auto") {
        return Ok(
            if matches!(
                panes,
                SnapshotPanes::Developer | SnapshotPanes::ResponseAndDeveloper
            ) {
                WaitCondition::EvaluationTerminal
            } else {
                WaitCondition::TaskTerminal
            },
        );
    }
    parse_wait_condition(value)
}

pub(crate) async fn run_driver_command(args: &Args, command: DriverArgs) -> miette::Result<()> {
    validate_lifecycle_options(&command)?;
    let cwd = driver_cwd(args)?;
    let transport = driver_transport(args, command.embedded, &cwd).await?;
    let mut driver = TuiDriver::launch(
        transport,
        command.session.as_deref().or(args.session_id.as_deref()),
        args.task_id.as_deref(),
        args.debug,
        args.yolo,
        command.width,
        command.height,
    )
    .await?;

    #[cfg(unix)]
    if let Some(socket) = command.socket.as_deref() {
        return run_socket_driver(&mut driver, socket, &command).await;
    }
    #[cfg(not(unix))]
    if command.socket.is_some() {
        return Err(miette::miette!(
            "unsupported_transport: Unix socket mode is unavailable on this platform"
        ));
    }

    serve_protocol(
        &mut driver,
        tokio::io::stdin(),
        tokio::io::stdout(),
        heartbeat_duration(command.heartbeat_secs),
        (command.idle_timeout_secs > 0).then(|| Duration::from_secs(command.idle_timeout_secs)),
    )
    .await
}

async fn driver_transport(
    args: &Args,
    embedded: bool,
    cwd: &Path,
) -> miette::Result<RuntimeTransport> {
    if embedded && (args.connect.is_some() || args.daemon) {
        return Err(miette::miette!(
            "invalid_transport: --embedded cannot be combined with --connect or --daemon"
        ));
    }
    let transport = if embedded {
        RuntimeTransport::for_cwd(cwd).await
    } else if let Some(base_url) = args.connect.clone() {
        RuntimeTransport::connect(base_url, cwd).await
    } else {
        RuntimeTransport::local_daemon(cwd).await
    };
    transport.map_err(|error| {
        let hint = if embedded || args.connect.is_some() {
            String::new()
        } else {
            "; start golutra-app-server or use --embedded for an isolated process".to_owned()
        };
        miette::miette!("runtime_transport: {error}{hint}")
    })
}

fn driver_cwd(args: &Args) -> miette::Result<PathBuf> {
    args.cwd
        .clone()
        .map_or_else(std::env::current_dir, Ok)
        .map_err(|error| miette::miette!("workspace: {error}"))
}

fn validate_lifecycle_options(command: &DriverArgs) -> miette::Result<()> {
    if command.idle_timeout_secs > 86_400 {
        return Err(miette::miette!(
            "invalid_timeout: idle timeout may not exceed 86400 seconds"
        ));
    }
    if command.heartbeat_secs > 3_600 {
        return Err(miette::miette!(
            "invalid_timeout: heartbeat may not exceed 3600 seconds"
        ));
    }
    Ok(())
}

fn heartbeat_duration(seconds: u64) -> Option<Duration> {
    (seconds > 0).then(|| Duration::from_secs(seconds))
}

async fn serve_protocol<R, W>(
    driver: &mut TuiDriver,
    reader: R,
    mut writer: W,
    heartbeat: Option<Duration>,
    idle_timeout: Option<Duration>,
) -> miette::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin,
{
    driver.record_connection();
    write_response(
        &mut writer,
        response(
            "ready",
            DriverResponse::Ready {
                ready: driver.ready().await?,
            },
        ),
    )
    .await?;

    let (mut incoming, reader_task) = spawn_reader(reader);
    let tick = Duration::from_millis(100);
    let mut ticker = tokio::time::interval(tick);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await;
    let mut next_heartbeat = heartbeat.map(|duration| tokio::time::Instant::now() + duration);
    let mut idle_deadline = idle_timeout.map(|duration| tokio::time::Instant::now() + duration);
    let mut last_sync_error = None;
    let mut pending_waits = Vec::new();

    let protocol_result: miette::Result<()> = async {
        loop {
            tokio::select! {
            incoming = incoming.recv() => {
                let Some(incoming) = incoming else {
                    break;
                };
                idle_deadline = idle_timeout.map(|duration| tokio::time::Instant::now() + duration);
                match incoming {
                    Incoming::Envelope(envelope) => {
                        driver.record_request();
                        if envelope.request_id.trim().is_empty() || envelope.request_id.len() > 128 {
                            driver.record_request_error();
                            write_response(
                                &mut writer,
                                response(
                                    "invalid-request-id",
                                    DriverResponse::Error {
                                        code: "invalid_request_id".to_owned(),
                                        message: "request_id must contain 1..=128 UTF-8 bytes".to_owned(),
                                    },
                                ),
                            )
                            .await?;
                            continue;
                        }
                        if pending_waits
                            .iter()
                            .any(|pending: &PendingWait| pending.request_id == envelope.request_id)
                        {
                            driver.record_request_error();
                            write_response(
                                &mut writer,
                                response(
                                    envelope.request_id,
                                    DriverResponse::Error {
                                        code: "duplicate_request_id".to_owned(),
                                        message: "request_id is already used by a pending wait"
                                            .to_owned(),
                                    },
                                ),
                            )
                            .await?;
                            continue;
                        }
                        let request_id = envelope.request_id;
                        match envelope.request {
                            DriverRequest::Wait { until, timeout_ms } => {
                                let timeout_ms = timeout_ms
                                    .unwrap_or(DEFAULT_WAIT_MILLIS)
                                    .min(MAX_WAIT_MILLIS);
                                let submission = driver.resolved_submission();
                                match driver.condition_met_for(&until, submission) {
                                    true => {
                                        let started_at = driver.start_wait_metrics();
                                        driver.finish_wait_metrics(started_at, false);
                                        write_response(
                                            &mut writer,
                                            response(
                                                request_id,
                                                wait_response(driver, until, false),
                                            ),
                                        )
                                        .await?;
                                    }
                                    false if timeout_ms == 0 => {
                                        let started_at = driver.start_wait_metrics();
                                        driver.finish_wait_metrics(started_at, true);
                                        write_response(
                                            &mut writer,
                                            response(
                                                request_id,
                                                wait_response(driver, until, true),
                                            ),
                                        )
                                        .await?;
                                    }
                                    false if pending_waits.len() >= MAX_PENDING_WAITS => {
                                        driver.record_request_error();
                                        write_response(
                                            &mut writer,
                                            response(
                                                request_id,
                                                DriverResponse::Error {
                                                    code: "too_many_pending_waits".to_owned(),
                                                    message: format!(
                                                        "at most {MAX_PENDING_WAITS} waits may be pending"
                                                    ),
                                                },
                                            ),
                                        )
                                        .await?;
                                    }
                                    false => {
                                        let started_at = driver.start_wait_metrics();
                                        pending_waits.push(PendingWait {
                                            request_id,
                                            condition: until,
                                            submission,
                                            deadline: tokio::time::Instant::now()
                                                + Duration::from_millis(timeout_ms),
                                            started_at,
                                        });
                                    }
                                }
                            }
                            DriverRequest::Metrics => {
                                write_response(
                                    &mut writer,
                                    response(
                                        request_id,
                                        DriverResponse::Metrics {
                                            metrics: driver.metrics(pending_waits.len()),
                                        },
                                    ),
                                )
                                .await?;
                            }
                            request => {
                                let response_value = driver.handle(request).await;
                                let closed = matches!(response_value, DriverResponse::Closed);
                                if matches!(response_value, DriverResponse::Error { .. }) {
                                    driver.record_request_error();
                                }
                                write_response(
                                    &mut writer,
                                    response(request_id, response_value),
                                )
                                .await?;
                                if let Some(notification) = driver.take_notification() {
                                    write_response(&mut writer, notification).await?;
                                }
                                if closed {
                                    break;
                                }
                            }
                        }
                    }
                    Incoming::Invalid(message) => {
                        driver.record_request();
                        driver.record_request_error();
                        write_response(
                            &mut writer,
                            response(
                                format!("invalid:{}", Uuid::now_v7()),
                                DriverResponse::Error {
                                    code: "invalid_request".to_owned(),
                                    message,
                                },
                            ),
                        )
                        .await?;
                    }
                    Incoming::Eof => break,
                }
            }
            _ = ticker.tick() => {
                if let Err(error) = sync_driver(driver).await {
                    let message = bounded_error(&error.to_string());
                    if last_sync_error.as_deref() != Some(message.as_str()) {
                        write_response(
                            &mut writer,
                            response(
                                format!("sync:{}", Uuid::now_v7()),
                                DriverResponse::Error {
                                    code: driver_error_code(&error),
                                    message: message.clone(),
                                },
                            ),
                        )
                        .await?;
                        last_sync_error = Some(message);
                    }
                } else if let Some(notification) = driver.take_notification() {
                    last_sync_error = None;
                    write_response(&mut writer, notification).await?;
                } else {
                    last_sync_error = None;
                }
                let now = tokio::time::Instant::now();
                if !pending_waits.is_empty() {
                    let wait_facts = driver.wait_facts();
                    let mut index = 0;
                    while index < pending_waits.len() {
                        let condition = pending_waits[index].condition.clone();
                        let submission = pending_waits[index].submission;
                        let timed_out = now >= pending_waits[index].deadline;
                        let met = driver.condition_met_with_facts(
                            wait_facts.as_ref(),
                            &condition,
                            submission,
                        );
                        if met || timed_out {
                            let pending = pending_waits.remove(index);
                            driver.finish_wait_metrics(pending.started_at, !met);
                            write_response(
                                &mut writer,
                                response(
                                    pending.request_id,
                                    wait_response(driver, pending.condition, !met),
                                ),
                            )
                            .await?;
                        } else {
                            index += 1;
                        }
                    }
                }
                if let Some(deadline) = next_heartbeat
                    && now >= deadline
                {
                    write_response(
                        &mut writer,
                        response(
                            format!("heartbeat:{}", Uuid::now_v7()),
                            DriverResponse::Event {
                                event: DriverNotification {
                                    kind: DriverNotificationKind::Heartbeat,
                                    sequence_no: driver.app.cursor,
                                    status: driver
                                        .app
                                        .projection
                                        .as_ref()
                                        .map(|projection| projection.status.into()),
                                },
                            },
                        ),
                    )
                    .await?;
                    next_heartbeat = heartbeat.map(|duration| now + duration);
                }
                if idle_deadline.is_some_and(|deadline| now >= deadline) {
                    driver.closed = true;
                    write_response(
                        &mut writer,
                        response("idle-timeout", DriverResponse::Closed),
                    )
                    .await?;
                    break;
                }
            }
        }
        }
        Ok(())
    }
    .await;
    for pending in pending_waits.drain(..) {
        driver.cancel_wait_metrics(pending.started_at);
    }
    reader_task.abort();
    protocol_result
}

struct PendingWait {
    request_id: String,
    condition: WaitCondition,
    submission: Option<super::SubmissionAnchor>,
    deadline: tokio::time::Instant,
    started_at: Instant,
}

fn wait_response(
    driver: &mut TuiDriver,
    condition: WaitCondition,
    timed_out: bool,
) -> DriverResponse {
    let state = driver.cached_state();
    if timed_out {
        DriverResponse::WaitTimeout { condition, state }
    } else {
        DriverResponse::WaitResult { condition, state }
    }
}

async fn sync_driver(driver: &mut TuiDriver) -> miette::Result<()> {
    tokio::time::timeout(DRIVER_SYNC_TIMEOUT, driver.sync())
        .await
        .map_err(|_| miette::miette!("runtime_sync_timeout: runtime synchronization timed out"))?
}

enum Incoming {
    Envelope(DriverEnvelope),
    Invalid(String),
    Eof,
}

fn spawn_reader<R>(reader: R) -> (mpsc::Receiver<Incoming>, tokio::task::JoinHandle<()>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let (sender, receiver) = mpsc::channel(32);
    let task = tokio::spawn(async move {
        let mut reader = BufReader::new(reader);
        loop {
            match read_bounded_line(&mut reader).await {
                Ok(Some(bytes)) => {
                    let incoming = match serde_json::from_slice::<DriverEnvelope>(&bytes) {
                        Ok(envelope) => Incoming::Envelope(envelope),
                        Err(error) => {
                            Incoming::Invalid(format!("request JSON is invalid: {error}"))
                        }
                    };
                    if sender.send(incoming).await.is_err() {
                        return;
                    }
                }
                Ok(None) => {
                    let _ = sender.send(Incoming::Eof).await;
                    return;
                }
                Err(error) => {
                    if sender.send(Incoming::Invalid(error)).await.is_err() {
                        return;
                    }
                }
            }
        }
    });
    (receiver, task)
}

async fn read_bounded_line<R>(reader: &mut R) -> Result<Option<Vec<u8>>, String>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = Vec::new();
    let mut overflow = false;
    loop {
        let available = reader
            .fill_buf()
            .await
            .map_err(|error| format!("read request: {error}"))?;
        if available.is_empty() {
            if line.is_empty() && !overflow {
                return Ok(None);
            }
            return if overflow {
                Err(format!("request exceeds {MAX_DRIVER_LINE_BYTES} bytes"))
            } else {
                Ok(Some(line))
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |index| index + 1);
        if !overflow {
            let content_end = newline.unwrap_or(consumed);
            let remaining = MAX_DRIVER_LINE_BYTES.saturating_sub(line.len());
            let copied = content_end.min(remaining);
            line.extend_from_slice(&available[..copied]);
            if copied < content_end || (newline.is_none() && consumed > remaining) {
                overflow = true;
            }
        }
        reader.consume(consumed);
        if newline.is_some() {
            if overflow {
                return Err(format!("request exceeds {MAX_DRIVER_LINE_BYTES} bytes"));
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(line));
        }
    }
}

async fn write_response<W>(writer: &mut W, envelope: DriverResponseEnvelope) -> miette::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut bytes = serde_json::to_vec(&envelope).map_err(|error| miette::miette!("{error}"))?;
    bytes.push(b'\n');
    writer
        .write_all(&bytes)
        .await
        .map_err(|error| miette::miette!("write driver response: {error}"))?;
    writer
        .flush()
        .await
        .map_err(|error| miette::miette!("flush driver response: {error}"))
}

fn parse_wait_condition(value: &str) -> miette::Result<WaitCondition> {
    let normalized = value.trim().to_ascii_lowercase().replace('_', "-");
    let condition = match normalized.as_str() {
        "ready" => WaitCondition::Ready,
        "idle" => WaitCondition::Idle,
        "task-started" => WaitCondition::TaskStarted,
        "task-terminal" => WaitCondition::TaskTerminal,
        "turn-terminal" => WaitCondition::TurnTerminal,
        "approval-required" => WaitCondition::ApprovalRequired,
        "authentication-required" | "auth-required" => WaitCondition::AuthenticationRequired,
        "evaluation-terminal" => WaitCondition::EvaluationTerminal,
        event if event.starts_with("event:") => {
            let mut parts = event[6..].split(':');
            let event_type = parts.next().unwrap_or_default().trim();
            if event_type.is_empty() {
                return Err(miette::miette!(
                    "invalid_wait: event wait requires an event type"
                ));
            }
            let sequence_at_least = parts
                .next()
                .map(|sequence| {
                    sequence.parse::<u64>().map_err(|error| {
                        miette::miette!("invalid_wait: event sequence is invalid: {error}")
                    })
                })
                .transpose()?;
            if parts.next().is_some() {
                return Err(miette::miette!(
                    "invalid_wait: expected event:EVENT_TYPE or event:EVENT_TYPE:SEQUENCE"
                ));
            }
            WaitCondition::Event {
                event_type: event_type.replace('-', "_"),
                sequence_at_least,
            }
        }
        _ => {
            return Err(miette::miette!(
                "invalid_wait: unsupported wait condition {value}"
            ));
        }
    };
    Ok(condition)
}

fn parse_view(value: &str) -> miette::Result<(SnapshotScope, SnapshotPanes)> {
    match value.trim().to_ascii_lowercase().as_str() {
        "response" | "turn" => Ok((SnapshotScope::CurrentTurn, SnapshotPanes::Transcript)),
        "response+developer" | "turn+developer" | "response-and-developer" => Ok((
            SnapshotScope::CurrentTurn,
            SnapshotPanes::ResponseAndDeveloper,
        )),
        "developer" => Ok((SnapshotScope::CurrentTurn, SnapshotPanes::Developer)),
        "task" => Ok((SnapshotScope::Task, SnapshotPanes::Transcript)),
        "task+developer" => Ok((SnapshotScope::Task, SnapshotPanes::ResponseAndDeveloper)),
        "session" => Ok((SnapshotScope::Session, SnapshotPanes::Transcript)),
        "screen" => Ok((SnapshotScope::Screen, SnapshotPanes::FullScreen)),
        _ => Err(miette::miette!(
            "invalid_view: expected response, response+developer, developer, task, task+developer, session, or screen"
        )),
    }
}

fn parse_detail(value: &str) -> miette::Result<SnapshotDetail> {
    match value.trim().to_ascii_lowercase().as_str() {
        "text" => Ok(SnapshotDetail::Text),
        "cells" => Ok(SnapshotDetail::Cells),
        _ => Err(miette::miette!("invalid_detail: expected text or cells")),
    }
}

fn parse_row_range(value: &str) -> miette::Result<RowRange> {
    let value = value.trim();
    let separator = [':', '-']
        .into_iter()
        .find(|separator| value.contains(*separator));
    let (start, end) = match separator {
        Some(separator) => value
            .split_once(separator)
            .ok_or_else(|| miette::miette!("invalid_rows: expected START:END"))?,
        None => (value, value),
    };
    let start = start
        .trim()
        .parse::<u32>()
        .map_err(|error| miette::miette!("invalid_rows: start is invalid: {error}"))?;
    let end = end
        .trim()
        .parse::<u32>()
        .map_err(|error| miette::miette!("invalid_rows: end is invalid: {error}"))?;
    if start == 0 || end < start {
        return Err(miette::miette!(
            "invalid_rows: rows are a 1-based inclusive range"
        ));
    }
    Ok(RowRange { start, end })
}

async fn write_inspect_output(format: &str, frame: TuiFrame) -> miette::Result<()> {
    let mut stdout = tokio::io::stdout();
    match format.trim().to_ascii_lowercase().as_str() {
        "json" => {
            let mut bytes = serde_json::to_vec_pretty(&frame)
                .map_err(|error| miette::miette!("serialize frame: {error}"))?;
            bytes.push(b'\n');
            stdout
                .write_all(&bytes)
                .await
                .map_err(|error| miette::miette!("write frame: {error}"))?;
        }
        "ndjson" => {
            write_response(
                &mut stdout,
                response("inspect", DriverResponse::Snapshot { frame }),
            )
            .await?;
        }
        "text" => {
            for line in frame.lines {
                stdout
                    .write_all(line.text.as_bytes())
                    .await
                    .map_err(|error| miette::miette!("write frame: {error}"))?;
                stdout
                    .write_all(b"\n")
                    .await
                    .map_err(|error| miette::miette!("write frame: {error}"))?;
            }
        }
        _ => {
            return Err(miette::miette!(
                "invalid_format: expected json, ndjson, or text"
            ));
        }
    }
    stdout
        .flush()
        .await
        .map_err(|error| miette::miette!("flush frame: {error}"))
}

fn unexpected_response(action: &str, response: DriverResponse) -> miette::Report {
    miette::miette!("driver_response: {action} returned {response:?}")
}

#[cfg(unix)]
async fn run_socket_driver(
    driver: &mut TuiDriver,
    socket_path: &Path,
    command: &DriverArgs,
) -> miette::Result<()> {
    prepare_socket_parent(socket_path).await?;
    let _lease = SocketLease::acquire(socket_path)?;
    prepare_socket_path(socket_path).await?;
    let listener = bind_secure_socket(socket_path)?;
    set_socket_permissions(socket_path)?;
    let _guard = SocketGuard(socket_path.to_path_buf());
    eprintln!("golutra-tui driver ready: {}", socket_path.display());

    let idle_timeout =
        (command.idle_timeout_secs > 0).then(|| Duration::from_secs(command.idle_timeout_secs));
    let heartbeat = heartbeat_duration(command.heartbeat_secs);
    let mut idle_deadline = idle_timeout.map(|duration| tokio::time::Instant::now() + duration);
    let mut last_sync_error = None;
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.map_err(|error| {
                    miette::miette!("accept driver socket {}: {error}", socket_path.display())
                })?;
                if let Err(error) = authenticate_socket_peer(&stream) {
                    driver.record_rejected_connection();
                    eprintln!(
                        "golutra-tui driver rejected a socket peer: {}",
                        bounded_error(&error.to_string())
                    );
                    continue;
                }
                let (reader, writer) = stream.into_split();
                if let Err(error) = serve_protocol(driver, reader, writer, heartbeat, idle_timeout).await {
                    eprintln!(
                        "golutra-tui driver connection closed with an error: {}",
                        bounded_error(&error.to_string())
                    );
                }
                if driver.closed {
                    return Ok(());
                }
                idle_deadline = idle_timeout.map(|duration| tokio::time::Instant::now() + duration);
            }
            _ = ticker.tick() => {
                if let Err(error) = sync_driver(driver).await {
                    let message = bounded_error(&error.to_string());
                    if last_sync_error.as_deref() != Some(message.as_str()) {
                        eprintln!("golutra-tui driver sync failed: {message}");
                        last_sync_error = Some(message);
                    }
                } else {
                    last_sync_error = None;
                }
                if idle_deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
                    driver.closed = true;
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(unix)]
fn authenticate_socket_peer(stream: &tokio::net::UnixStream) -> miette::Result<()> {
    let credentials = stream
        .peer_cred()
        .map_err(|error| miette::miette!("peer_credential_failed: {error}"))?;
    validate_peer_uid(credentials.uid(), nix::unistd::geteuid().as_raw())
}

#[cfg(unix)]
fn validate_peer_uid(peer_uid: u32, expected_uid: u32) -> miette::Result<()> {
    if peer_uid == expected_uid {
        Ok(())
    } else {
        Err(miette::miette!(
            "peer_uid_mismatch: peer UID {peer_uid} does not match Driver UID {expected_uid}"
        ))
    }
}

#[cfg(unix)]
fn bind_secure_socket(path: &Path) -> miette::Result<tokio::net::UnixListener> {
    use std::os::unix::net::UnixListener as StdUnixListener;

    // The socket path becomes visible as soon as bind returns. Restrict the
    // creation umask so clients can never observe the platform default mode
    // during the short interval before the explicit permission check below.
    let listener = {
        let _umask = RestrictiveUmask::new();
        StdUnixListener::bind(path)
            .map_err(|error| miette::miette!("bind driver socket {}: {error}", path.display()))?
    };
    listener
        .set_nonblocking(true)
        .map_err(|error| miette::miette!("configure driver socket: {error}"))?;
    tokio::net::UnixListener::from_std(listener)
        .map_err(|error| miette::miette!("register driver socket: {error}"))
}

#[cfg(unix)]
struct RestrictiveUmask(nix::libc::mode_t);

#[cfg(unix)]
impl RestrictiveUmask {
    fn new() -> Self {
        // Unix domain sockets are created with a platform-dependent default
        // mode. 0177 reduces that mode to owner read/write (0600).
        Self(unsafe { nix::libc::umask(0o177) })
    }
}

#[cfg(unix)]
impl Drop for RestrictiveUmask {
    fn drop(&mut self) {
        unsafe {
            nix::libc::umask(self.0);
        }
    }
}

#[cfg(unix)]
async fn prepare_socket_path(path: &Path) -> miette::Result<()> {
    use std::os::unix::fs::FileTypeExt;

    prepare_socket_parent(path).await?;
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.file_type().is_socket() {
                return Err(miette::miette!(
                    "invalid_socket: existing path is not a Unix socket: {}",
                    path.display()
                ));
            }
            match tokio::net::UnixStream::connect(path).await {
                Ok(_) => {
                    return Err(miette::miette!(
                        "socket_in_use: a driver is already listening at {}",
                        path.display()
                    ));
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                    ) =>
                {
                    tokio::fs::remove_file(path)
                        .await
                        .map_err(|error| miette::miette!("remove stale driver socket: {error}"))?;
                }
                Err(error) => return Err(miette::miette!("inspect driver socket: {error}")),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(miette::miette!("inspect driver socket: {error}")),
    }
    Ok(())
}

#[cfg(unix)]
async fn prepare_socket_parent(path: &Path) -> miette::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            miette::miette!("invalid_socket: socket path must have a parent directory")
        })?;
    match tokio::fs::symlink_metadata(parent).await {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(miette::miette!(
                    "invalid_socket: parent is not a real directory: {}",
                    parent.display()
                ));
            }
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(miette::miette!(
                    "invalid_socket: parent directory must be owner-only (0700): {}",
                    parent.display()
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| miette::miette!("create socket directory: {error}"))?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .map_err(|error| miette::miette!("secure socket directory: {error}"))?;
        }
        Err(error) => return Err(miette::miette!("inspect socket directory: {error}")),
    }

    Ok(())
}

#[cfg(unix)]
fn set_socket_permissions(path: &Path) -> miette::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| miette::miette!("secure driver socket: {error}"))
}

#[cfg(unix)]
struct SocketGuard(PathBuf);

#[cfg(unix)]
struct SocketLease {
    _file: File,
}

#[cfg(unix)]
impl SocketLease {
    fn acquire(socket_path: &Path) -> miette::Result<Self> {
        use std::os::unix::fs::PermissionsExt;

        let file_name = socket_path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| miette::miette!("invalid_socket: socket file name is not UTF-8"))?;
        let lock_path = socket_path.with_file_name(format!("{file_name}.lock"));
        match std::fs::symlink_metadata(&lock_path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(miette::miette!(
                    "invalid_socket: lock path is not a regular file: {}",
                    lock_path.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(miette::miette!("inspect driver lock: {error}")),
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|error| miette::miette!("open driver lock: {error}"))?;
        std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| miette::miette!("secure driver lock: {error}"))?;
        file.try_lock_exclusive().map_err(|error| {
            miette::miette!(
                "socket_in_use: another driver owns {}: {error}",
                socket_path.display()
            )
        })?;
        Ok(Self { _file: file })
    }
}

#[cfg(unix)]
impl Drop for SocketGuard {
    fn drop(&mut self) {
        use std::os::unix::fs::FileTypeExt;

        if std::fs::symlink_metadata(&self.0).is_ok_and(|metadata| metadata.file_type().is_socket())
        {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

#[cfg(test)]
mod tests;
