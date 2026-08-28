//! 文件缓存使用的低成本身份计算。

use std::{fs, path::Path};

/// 内容只在身份变化后读取。Unix 的 ctime/inode 能识别保留 mtime 的
/// 同尺寸替换；其他平台使用各自可稳定取得的最强元数据组合。
pub(crate) fn metadata_fingerprint(metadata: &fs::Metadata) -> String {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |value| value.as_nanos());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        format!(
            "{}:{}:{}:{}:{}:{}",
            metadata.len(),
            modified,
            metadata.dev(),
            metadata.ino(),
            metadata.ctime(),
            metadata.ctime_nsec()
        )
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        format!(
            "{}:{}:{}:{}:{}",
            metadata.len(),
            modified,
            metadata.creation_time(),
            metadata.last_write_time(),
            metadata.file_attributes()
        )
    }
    #[cfg(not(any(unix, windows)))]
    format!("{}:{}", metadata.len(), modified)
}

pub(crate) fn path_metadata_fingerprint(path: &Path) -> String {
    match fs::metadata(path) {
        Ok(metadata) => metadata_fingerprint(&metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "missing".to_owned(),
        Err(error) => format!("error:{}", error.kind()),
    }
}
