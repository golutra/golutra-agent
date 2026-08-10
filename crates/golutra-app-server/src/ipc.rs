#[cfg(unix)]
use std::{collections::BTreeMap, io, sync::Arc, time::Duration};

#[cfg(unix)]
use axum::{
    Router,
    body::Body,
    http::{HeaderName, HeaderValue, Method, Request, Uri, header},
};
#[cfg(unix)]
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
#[cfg(unix)]
use futures_util::StreamExt;
#[cfg(unix)]
use golutra_protocol::{IpcHttpRequest, IpcHttpResponseFrame, MAX_WIRE_MESSAGE_BYTES};
#[cfg(unix)]
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::Semaphore,
};
#[cfg(unix)]
use tower::ServiceExt;

/// Marks requests that arrived over the owner-only Unix socket.
///
/// The HTTP and IPC servers share one Axum router, so disclosure-sensitive
/// handlers use this extension instead of trusting a spoofable request header.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LocalIpcRequest;

#[cfg(unix)]
const INITIAL_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(unix)]
const RESPONSE_CHUNK_BYTES: usize = 48 * 1024;
#[cfg(unix)]
const MAX_CONNECTIONS: usize = 128;

#[cfg(unix)]
pub async fn serve(listener: UnixListener, app: Router) -> io::Result<()> {
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let (stream, _) = listener.accept().await?;
        let permit = permits
            .clone()
            .acquire_owned()
            .await
            .map_err(io::Error::other)?;
        let app = app.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let _ = handle_connection(stream, app).await;
        });
    }
}

#[cfg(unix)]
async fn handle_connection(stream: UnixStream, app: Router) -> io::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    let read = read_initial_request_line(
        &mut reader,
        &mut line,
        MAX_WIRE_MESSAGE_BYTES,
        INITIAL_REQUEST_TIMEOUT,
    )
    .await?;
    if read == 0 {
        return Ok(());
    }
    if !line.ends_with(b"\n") || line.len().saturating_sub(1) > MAX_WIRE_MESSAGE_BYTES {
        write_frame(
            &mut writer,
            &IpcHttpResponseFrame::Error {
                message: "IPC request exceeds its framing limit".to_owned(),
            },
        )
        .await?;
        return Ok(());
    }
    let request = match serde_json::from_slice::<IpcHttpRequest>(&line) {
        Ok(request) => request,
        Err(error) => {
            write_frame(
                &mut writer,
                &IpcHttpResponseFrame::Error {
                    message: format!("IPC request JSON is invalid: {error}"),
                },
            )
            .await?;
            return Ok(());
        }
    };
    let request = match axum_request(request) {
        Ok(request) => request,
        Err(error) => {
            write_frame(&mut writer, &IpcHttpResponseFrame::Error { message: error }).await?;
            return Ok(());
        }
    };
    let response = app.oneshot(request).await.expect("router is infallible");
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_owned(), value.to_owned()))
        })
        .take(32)
        .collect::<BTreeMap<_, _>>();
    write_frame(&mut writer, &IpcHttpResponseFrame::Head { status, headers }).await?;
    let mut stream = response.into_body().into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                write_frame(
                    &mut writer,
                    &IpcHttpResponseFrame::Error {
                        message: format!("IPC response body failed: {error}"),
                    },
                )
                .await?;
                return Ok(());
            }
        };
        for chunk in chunk.chunks(RESPONSE_CHUNK_BYTES) {
            write_frame(
                &mut writer,
                &IpcHttpResponseFrame::Chunk {
                    data_base64: BASE64.encode(chunk),
                },
            )
            .await?;
        }
    }
    write_frame(&mut writer, &IpcHttpResponseFrame::End).await
}

#[cfg(unix)]
async fn read_bounded_line<R>(reader: &mut R, line: &mut Vec<u8>, limit: usize) -> io::Result<usize>
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

#[cfg(unix)]
fn axum_request(request: IpcHttpRequest) -> Result<Request<Body>, String> {
    if request.path.len() > 16 * 1024 {
        return Err("IPC request path is too long".to_owned());
    }
    let method = request
        .method
        .parse::<Method>()
        .map_err(|_| "IPC request method is invalid".to_owned())?;
    if !matches!(method, Method::GET | Method::POST | Method::DELETE) {
        return Err("IPC request method is not allowed".to_owned());
    }
    let uri = request
        .path
        .parse::<Uri>()
        .map_err(|_| "IPC request URI is invalid".to_owned())?;
    if uri.scheme().is_some() || uri.authority().is_some() || !uri.path().starts_with('/') {
        return Err("IPC request URI must be an absolute path".to_owned());
    }
    if request.headers.len() > 16 {
        return Err("IPC request contains too many headers".to_owned());
    }
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "localhost");
    for (name, value) in request.headers {
        let normalized = name.to_ascii_lowercase();
        if !matches!(
            normalized.as_str(),
            "authorization"
                | "content-type"
                | "last-event-id"
                | "x-golutra-attachment"
                | "x-golutra-actor-id"
                | "x-golutra-protocol-version"
        ) {
            return Err(format!("IPC request header `{name}` is not allowed"));
        }
        let name = HeaderName::from_bytes(normalized.as_bytes())
            .map_err(|_| "IPC request header name is invalid".to_owned())?;
        let value = HeaderValue::from_str(&value)
            .map_err(|_| "IPC request header value is invalid".to_owned())?;
        builder = builder.header(name, value);
    }
    let body = request
        .body
        .map(|body| serde_json::to_vec(&body))
        .transpose()
        .map_err(|error| format!("IPC request body is invalid: {error}"))?
        .unwrap_or_default();
    if body.len() > MAX_WIRE_MESSAGE_BYTES {
        return Err("IPC request body exceeds its size limit".to_owned());
    }
    if !body.is_empty() {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    let mut request = builder
        .body(Body::from(body))
        .map_err(|error| format!("IPC request could not be built: {error}"))?;
    request.extensions_mut().insert(LocalIpcRequest);
    Ok(request)
}

#[cfg(unix)]
async fn read_initial_request_line<R>(
    reader: &mut R,
    line: &mut Vec<u8>,
    limit: usize,
    deadline: Duration,
) -> io::Result<usize>
where
    R: AsyncBufRead + Unpin,
{
    tokio::time::timeout(deadline, read_bounded_line(reader, line, limit))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "IPC request first frame timed out"))?
}

#[cfg(unix)]
async fn write_frame(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    frame: &IpcHttpResponseFrame,
) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(frame).map_err(io::Error::other)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[tokio::test]
    async fn bounded_line_reader_accepts_a_newline_after_the_limit_bytes() {
        let mut input = vec![b'x'; 16];
        input.extend_from_slice(b"\nremaining");
        let mut reader = BufReader::new(Cursor::new(input));
        let mut line = Vec::new();

        let read = read_bounded_line(&mut reader, &mut line, 16)
            .await
            .expect("bounded line");

        assert_eq!(read, 17);
        assert_eq!(line, [vec![b'x'; 16], vec![b'\n']].concat());
        assert_eq!(reader.fill_buf().await.expect("remaining"), b"remaining");
    }

    #[tokio::test]
    async fn bounded_line_reader_stops_at_limit_plus_one_without_a_newline() {
        let mut reader = BufReader::new(Cursor::new(vec![b'x'; 64]));
        let mut line = Vec::new();

        let read = read_bounded_line(&mut reader, &mut line, 16)
            .await
            .expect("bounded line");

        assert_eq!(read, 17);
        assert_eq!(line.len(), 17);
        assert_eq!(reader.fill_buf().await.expect("remaining").len(), 47);
    }

    #[tokio::test]
    async fn initial_request_reader_times_out_without_a_first_frame() {
        let (_writer, reader) = tokio::io::duplex(64);
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();

        let error = read_initial_request_line(
            &mut reader,
            &mut line,
            MAX_WIRE_MESSAGE_BYTES,
            Duration::from_millis(20),
        )
        .await
        .expect_err("initial frame deadline");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn ipc_request_body_uses_the_protocol_wire_limit() {
        let request = IpcHttpRequest {
            method: "POST".to_owned(),
            path: "/rpc".to_owned(),
            headers: BTreeMap::new(),
            body: Some(serde_json::Value::String(
                "x".repeat(MAX_WIRE_MESSAGE_BYTES),
            )),
        };

        assert!(matches!(
            axum_request(request),
            Err(message) if message == "IPC request body exceeds its size limit"
        ));
    }
}
