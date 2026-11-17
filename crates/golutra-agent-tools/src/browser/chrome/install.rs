//! 随 Agent 内嵌扩展资源；注册用户级 Native Messaging，不修改 Chrome 用户配置。
use super::super::state::{io_error, private_directory};
use super::transport::EXTENSION_ID;
use crate::ToolError;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const ASSETS: &[(&str, &str)] = &[
    (
        "frames.js",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../extensions/chrome/frames.js"
        )),
    ),
    (
        "manifest.json",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../extensions/chrome/manifest.json"
        )),
    ),
    (
        "background.js",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../extensions/chrome/background.js"
        )),
    ),
    (
        "page.js",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../extensions/chrome/page.js"
        )),
    ),
    (
        "popup.html",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../extensions/chrome/popup.html"
        )),
    ),
    (
        "popup.js",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../extensions/chrome/popup.js"
        )),
    ),
];

pub fn user_home() -> Result<PathBuf, ToolError> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or_else(|| ToolError::Execution("Cannot resolve user home".into()))
}

pub fn connection_dir() -> Result<PathBuf, ToolError> {
    Ok(user_home()?.join(".golutra-agent/chrome-bridge"))
}

fn manifest_directory(agent_home: &Path) -> Result<PathBuf, ToolError> {
    let user = user_home()?;
    Ok(if cfg!(target_os = "macos") {
        user.join("Library/Application Support/Google/Chrome/NativeMessagingHosts")
    } else if cfg!(target_os = "windows") {
        agent_home.join("browser/native-hosts")
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| user.join(".config"))
            .join("google-chrome/NativeMessagingHosts")
    })
}

/// 只检查本地安装材料，不尝试读取用户 Profile 或自动接管标签页。
pub fn diagnose(agent_home: &Path) -> Result<Value, ToolError> {
    let extension = agent_home.join("browser/chrome-extension");
    let missing_or_outdated: Vec<_> = ASSETS
        .iter()
        .filter_map(|(name, expected)| {
            (std::fs::read(extension.join(name)).ok().as_deref() != Some(expected.as_bytes()))
                .then_some(*name)
        })
        .collect();
    let manifest_path = manifest_directory(agent_home)?.join("com.golutra.browser.json");
    let manifest: Option<Value> = std::fs::read(&manifest_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let valid_manifest = manifest.as_ref().is_some_and(|m| {
        m["name"] == "com.golutra.browser"
            && m["type"] == "stdio"
            && m["allowed_origins"] == json!([format!("chrome-extension://{EXTENSION_ID}/")])
            && m["path"]
                .as_str()
                .is_some_and(|path| Path::new(path).is_absolute() && Path::new(path).is_file())
    });
    Ok(
        json!({"status":if missing_or_outdated.is_empty() && valid_manifest {"installed"} else {"repair_required"},
        "extension_directory":extension,"native_manifest":manifest_path,"native_manifest_valid":valid_manifest,
        "missing_or_outdated_assets":missing_or_outdated,
        "next":if missing_or_outdated.is_empty() && valid_manifest {
            "本地文件已就绪。请在 chrome://extensions 加载或重新加载扩展；扩展默认自动连接并允许新建任务页，无需额外开启；操作已有网页仍需单页授权。Windows 还需安装命令成功完成注册表注册。"
        } else {"运行 golutra-agent browser chrome install 修复安装，然后在 chrome://extensions 加载或重新加载扩展。"}}),
    )
}

pub fn install(agent_home: &Path, executable: &Path) -> Result<Value, ToolError> {
    let extension = agent_home.join("browser/chrome-extension");
    private_directory(&extension)?;
    for (name, body) in ASSETS {
        write_atomic(&extension.join(name), body.as_bytes())?;
    }
    // 仓库可能在 macOS 受保护的 Desktop/Documents；Chrome 子进程不能依赖该路径。
    // 复制到稳定的用户组件目录，也避免 npm 更新途中让注册指向不存在的文件。
    let native_directory = agent_home.join("browser/native-host");
    private_directory(&native_directory)?;
    let native_executable = native_directory.join(if cfg!(windows) {
        "golutra-browser-host.exe"
    } else {
        "golutra-browser-host"
    });
    let temporary = tempfile::NamedTempFile::new_in(&native_directory).map_err(io_error)?;
    std::fs::copy(executable, temporary.path()).map_err(io_error)?;
    temporary.as_file().sync_all().map_err(io_error)?;
    temporary
        .persist(&native_executable)
        .map_err(|e| io_error(e.error))?;
    let host_directory = manifest_directory(agent_home)?;
    private_directory(&host_directory)?;
    let manifest = host_directory.join("com.golutra.browser.json");
    let body = json!({"name":"com.golutra.browser","description":"Golutra browser connector",
        "path":native_executable.canonicalize().map_err(io_error)?,"type":"stdio",
        "allowed_origins":[format!("chrome-extension://{EXTENSION_ID}/")]});
    write_atomic(
        &manifest,
        &serde_json::to_vec_pretty(&body).map_err(|e| ToolError::Execution(e.to_string()))?,
    )?;
    #[cfg(windows)]
    {
        let status = std::process::Command::new("reg.exe")
            .args([
                "ADD",
                r"HKCU\Software\Google\Chrome\NativeMessagingHosts\com.golutra.browser",
                "/ve",
                "/t",
                "REG_SZ",
                "/d",
            ])
            .arg(&manifest)
            .arg("/f")
            .status()
            .map_err(io_error)?;
        if !status.success() {
            return Err(ToolError::Execution(
                "Native Messaging registry registration failed".into(),
            ));
        }
    }
    Ok(
        json!({"extension_directory":extension,"native_manifest":manifest,"extension_id":EXTENSION_ID,
        "next":"在 chrome://extensions 开启开发者模式，加载上面的扩展目录（升级后点击重新加载）。扩展默认自动连接本机并允许新建任务页，无需点击连接或开启按钮。操作已有网页仍可选择「使用当前标签页」。运行 golutra-agent browser connect chrome 保存配置，用 golutra-agent browser status 检查状态。"}),
    )
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ToolError> {
    use std::io::Write;
    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap()).map_err(io_error)?;
    file.write_all(bytes).map_err(io_error)?;
    file.flush().map_err(io_error)?;
    file.persist(path).map_err(|e| io_error(e.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn embedded_extension_manifest_matches_native_origin_and_has_no_broad_host_permissions() {
        use base64::Engine;
        use sha2::{Digest, Sha256};
        let manifest: Value = serde_json::from_str(
            ASSETS
                .iter()
                .find(|(name, _)| *name == "manifest.json")
                .unwrap()
                .1,
        )
        .unwrap();
        let key = base64::engine::general_purpose::STANDARD
            .decode(manifest["key"].as_str().unwrap())
            .unwrap();
        let digest = Sha256::digest(key);
        let id: String = digest[..16]
            .iter()
            .flat_map(|byte| {
                [
                    char::from(b'a' + (byte >> 4)),
                    char::from(b'a' + (byte & 15)),
                ]
            })
            .collect();
        assert_eq!(id, EXTENSION_ID);
        assert!(manifest.get("host_permissions").is_none());
        assert_eq!(manifest["manifest_version"], 3);
    }
}
