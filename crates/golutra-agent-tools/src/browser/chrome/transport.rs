//! Native Messaging 与本机回环连接桥接；令牌仅保存在用户私有目录。
use super::super::state::{io_error, private_directory, read_json};
use crate::ToolError;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    path::Path,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;

pub const EXTENSION_ID: &str = "eeefgmejgclnckofpckhcponhikcpdlc";
const MAX_FRAME: usize = 8 * 1024 * 1024;
const MAX_REQUEST: usize = 256 * 1024;

#[derive(Serialize, Deserialize)]
struct Endpoint {
    port: u16,
    token: String,
}

struct EndpointCleanup(std::path::PathBuf);
impl Drop for EndpointCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub async fn exchange(
    root: &Path,
    request: Value,
    cancel: CancellationToken,
) -> Result<Value, ToolError> {
    let endpoint: Endpoint = read_json(&root.join("endpoint.json")).map_err(|_| ToolError::Execution(
        "chrome_disconnected: 打开 Chrome 扩展检查本机连接，开启自动任务或授权已有标签页，再重试观察；不要重放未确认的操作。".into()))?;
    let operation = async {
        let mut stream = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, endpoint.port))
            .await
            .map_err(io_error)?;
        let bytes = serde_json::to_vec(&json!({"token":endpoint.token,"request":request}))
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        if bytes.len() > MAX_REQUEST {
            return Err(ToolError::InvalidArguments(
                "Chrome request too large".into(),
            ));
        }
        stream
            .write_u32_le(bytes.len() as u32)
            .await
            .map_err(io_error)?;
        stream.write_all(&bytes).await.map_err(io_error)?;
        let length = stream.read_u32_le().await.map_err(io_error)? as usize;
        if length > MAX_FRAME {
            return Err(ToolError::Execution("Chrome response too large".into()));
        }
        let mut bytes = vec![0; length];
        stream.read_exact(&mut bytes).await.map_err(io_error)?;
        serde_json::from_slice(&bytes).map_err(|e| ToolError::Execution(e.to_string()))
    };
    tokio::select! {
        _ = cancel.cancelled() => Err(ToolError::Execution("chrome_cancelled: action outcome may be unknown; inspect before retrying".into())),
        result = tokio::time::timeout(Duration::from_secs(35), operation) => result.unwrap_or_else(|_| Err(ToolError::Execution("chrome_timeout: action outcome may be unknown; inspect before retrying".into()))),
    }
}

fn read_native(reader: &mut impl Read) -> std::io::Result<Value> {
    let mut length = [0; 4];
    reader.read_exact(&mut length)?;
    let size = u32::from_le_bytes(length) as usize;
    if size > MAX_FRAME {
        return Err(std::io::Error::other("native frame exceeds limit"));
    }
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes).map_err(std::io::Error::other)
}

fn write_native(value: &Value) -> Result<(), ToolError> {
    let bytes = serde_json::to_vec(value).map_err(|e| ToolError::Execution(e.to_string()))?;
    if bytes.len() > MAX_REQUEST {
        return Err(ToolError::Execution("native request exceeds limit".into()));
    }
    let mut out = std::io::stdout().lock();
    out.write_all(&(bytes.len() as u32).to_le_bytes())
        .map_err(io_error)?;
    out.write_all(&bytes).map_err(io_error)?;
    out.flush().map_err(io_error)
}

pub async fn serve(root: &Path, origin: &str) -> Result<(), ToolError> {
    if origin != format!("chrome-extension://{EXTENSION_ID}/") {
        return Err(ToolError::Execution(
            "unregistered Chrome extension origin".into(),
        ));
    }
    private_directory(root)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("host.lock"))
        .map_err(io_error)?;
    lock.try_lock_exclusive().map_err(|_| {
        ToolError::Execution("另一个 Chrome 配置已连接；请先断开它，再连接当前配置。".into())
    })?;
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(io_error)?;
    let endpoint = Endpoint {
        port: listener.local_addr().map_err(io_error)?.port(),
        token: uuid::Uuid::new_v4().to_string(),
    };
    let mut file = tempfile::NamedTempFile::new_in(root).map_err(io_error)?;
    serde_json::to_writer(&mut file, &endpoint).map_err(|e| ToolError::Execution(e.to_string()))?;
    file.flush().map_err(io_error)?;
    file.persist(root.join("endpoint.json"))
        .map_err(|e| io_error(e.error))?;
    let _endpoint_cleanup = EndpointCleanup(root.join("endpoint.json"));
    let (tx, mut rx) = mpsc::channel(4);
    // 阻塞 stdin 独立于异步 socket；EOF 立即唤醒主循环，进程退出不遗留宿主服务。
    std::thread::spawn(move || {
        let mut input = std::io::stdin().lock();
        loop {
            let value = read_native(&mut input);
            let done = value.is_err();
            if tx.blocking_send(value).is_err() || done {
                break;
            }
        }
    });
    let hello = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .map_err(|_| ToolError::Execution("extension handshake timed out".into()))?
        .ok_or_else(|| ToolError::Execution("extension disconnected before handshake".into()))?
        .map_err(io_error)?;
    if hello != json!({"type":"hello","version":1}) {
        return Err(ToolError::Execution(
            "unsupported extension handshake".into(),
        ));
    }
    write_native(&json!({"type":"ready","version":1}))?;
    let result = async {
        loop {
            let (mut stream, _) = tokio::select! {
                _ = rx.recv() => return Ok(()),
                accepted = listener.accept() => accepted.map_err(io_error)?,
            };
            let input = tokio::time::timeout(Duration::from_secs(2), async {
                let length = stream.read_u32_le().await.map_err(io_error)? as usize;
                if length > MAX_REQUEST {
                    return Err(ToolError::Execution("request exceeds limit".into()));
                }
                let mut bytes = vec![0; length];
                stream.read_exact(&mut bytes).await.map_err(io_error)?;
                serde_json::from_slice::<Value>(&bytes)
                    .map_err(|e| ToolError::Execution(e.to_string()))
            })
            .await;
            let Ok(Ok(input)) = input else {
                continue;
            };
            if input["token"].as_str() != Some(&endpoint.token) {
                continue;
            }
            let request = &input["request"];
            write_native(request)?;
            let response = tokio::time::timeout(Duration::from_secs(30), rx.recv())
                .await
                .map_err(|_| {
                    ToolError::Execution(
                        "native extension timed out; connection closed to prevent replay".into(),
                    )
                })?
                .ok_or_else(|| ToolError::Execution("extension disconnected".into()))?
                .map_err(io_error)?;
            if response["id"] != request["id"] {
                return Err(ToolError::Execution("native response ID mismatch".into()));
            }
            let bytes =
                serde_json::to_vec(&response).map_err(|e| ToolError::Execution(e.to_string()))?;
            // 调用者取消也必须先收取此次结果，再接受下一次请求，避免错配。
            let _ = stream.write_u32_le(bytes.len() as u32).await;
            let _ = stream.write_all(&bytes).await;
        }
    }
    .await;
    let _ = std::fs::remove_file(root.join("endpoint.json"));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_frames_reject_truncation_and_oversized_allocation() {
        assert!(read_native(&mut std::io::Cursor::new([255; 4])).is_err());
        assert!(read_native(&mut std::io::Cursor::new([2, 0, 0, 0, b'{'])).is_err());
        let mut bytes = vec![2, 0, 0, 0];
        bytes.extend(b"{}");
        assert_eq!(
            read_native(&mut std::io::Cursor::new(bytes)).unwrap(),
            json!({})
        );
    }
}
