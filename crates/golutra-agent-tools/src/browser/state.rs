use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use super::super::{ToolError, ToolRequest};

const MAX_STATE_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BrowserState {
    pub version: u32,
    pub session_id: String,
    pub workspace: PathBuf,
    pub executable: PathBuf,
    pub generation: u64,
    pub space_name: String,
    pub space_id: Option<u64>,
    pub phase: String,
    pub last_call: String,
    // 旧状态不推断保留页面的所有权；只有已验证的结束回执允许重新 claim。
    #[serde(default)]
    pub reclaimable: bool,
    #[serde(default)]
    pub reclaim_pending: bool,
    #[serde(default)]
    pub default_page: Option<String>,
    #[serde(default)]
    pub requires_reload: bool,
}

pub(super) struct StateLease {
    _lock: File,
    pub directory: PathBuf,
    pub path: PathBuf,
}

impl Drop for StateLease {
    fn drop(&mut self) {
        // flock 随打开的文件描述而非 Rust 句柄存活；并发 fork 到 exec 之间可能继承它。
        // 显式解锁避免已完成的调用仍被无关子进程占用；失败时 File::drop 仍关闭本地句柄。
        let _ = FileExt::unlock(&self._lock);
    }
}

impl StateLease {
    pub fn acquire(root: &Path, request: &ToolRequest) -> Result<Self, ToolError> {
        let directory = root.join(request.session_id.to_string());
        private_directory(&directory)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join("lease.lock"))
            .map_err(io_error)?;
        lock.try_lock_exclusive().map_err(|_| ToolError::Execution(
            "browser_busy: another invocation owns this session's browser; do not run concurrently".to_owned()))?;
        Ok(Self {
            _lock: lock,
            path: directory.join("state.json"),
            directory,
        })
    }

    pub fn read(
        &self,
        request: &ToolRequest,
        workspace: &Path,
        executable: &Path,
    ) -> Result<BrowserState, ToolError> {
        if !self.path.exists() {
            return Ok(BrowserState {
                version: 1,
                session_id: request.session_id.to_string(),
                workspace: workspace.to_path_buf(),
                executable: executable.to_path_buf(),
                generation: 0,
                space_name: String::new(),
                space_id: None,
                phase: "idle".to_owned(),
                last_call: String::new(),
                reclaimable: false,
                reclaim_pending: false,
                default_page: None,
                requires_reload: false,
            });
        }
        let state: BrowserState = read_json(&self.path)?;
        if state.version != 1
            || state.session_id != request.session_id.to_string()
            || state.workspace != workspace
            || state.executable != executable
        {
            return Err(ToolError::Execution("browser_state_mismatch: refusing to reuse another session, workspace or browser binding".to_owned()));
        }
        Ok(state)
    }

    pub fn write(&self, state: &BrowserState) -> Result<(), ToolError> {
        let mut temp = tempfile::NamedTempFile::new_in(&self.directory).map_err(io_error)?;
        serde_json::to_writer(&mut temp, state)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        temp.flush().map_err(io_error)?;
        temp.as_file().sync_all().map_err(io_error)?;
        temp.persist(&self.path)
            .map_err(|error| io_error(error.error))?;
        Ok(())
    }
}

pub(super) fn private_directory(path: &Path) -> Result<(), ToolError> {
    std::fs::create_dir_all(path).map_err(io_error)?;
    let metadata = std::fs::symlink_metadata(path).map_err(io_error)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(ToolError::Execution(
            "browser state must use real directories, not symlinks".to_owned(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(io_error)?;
    }
    Ok(())
}

pub(super) fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, ToolError> {
    let metadata = std::fs::symlink_metadata(path).map_err(io_error)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(ToolError::Execution(
            "browser state/receipt must be a regular file".to_owned(),
        ));
    }
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(io_error)?
        .take(MAX_STATE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err(ToolError::Execution(
            "browser state/receipt exceeds 64 KiB".to_owned(),
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| ToolError::Execution(format!("invalid browser state/receipt: {error}")))
}

pub(super) fn io_error(error: std::io::Error) -> ToolError {
    ToolError::Execution(format!("browser I/O: {error}"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use golutra_agent_core::{SessionId, ToolCallId};

    #[test]
    fn lease_release_does_not_wait_for_an_inherited_file_description() {
        let root = tempfile::tempdir().unwrap();
        let request = ToolRequest {
            tool_call_id: ToolCallId::new(),
            provider_tool_call_id: None,
            session_id: SessionId::new(),
            turn_id: None,
            tool_name: "browser_run".into(),
            arguments: serde_json::json!({}),
        };
        let lease = StateLease::acquire(root.path(), &request).unwrap();
        // dup 与 fork 继承共享同一个 flock 所属文件描述；不引入依赖调度的时间窗口。
        let inherited = lease._lock.try_clone().unwrap();
        assert!(StateLease::acquire(root.path(), &request).is_err());
        drop(lease);
        let next = StateLease::acquire(root.path(), &request)
            .expect("completed call must release its lease");
        drop(inherited);
        assert!(StateLease::acquire(root.path(), &request).is_err());
        drop(next);
        assert!(StateLease::acquire(root.path(), &request).is_ok());
    }
}
