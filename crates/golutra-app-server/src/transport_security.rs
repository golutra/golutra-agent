use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::Path,
};

use axum::http::{HeaderMap, header};
use golutra_client::{APP_SERVER_TRANSPORT_TOKEN_ENV, AppServerPaths};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const MAX_TRANSPORT_TOKEN_BYTES: u64 = 1024;

#[derive(Clone)]
pub(crate) struct TransportAuth {
    token_digest: [u8; 32],
}

impl std::fmt::Debug for TransportAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TransportAuth")
            .field("configured", &true)
            .finish()
    }
}

impl TransportAuth {
    pub(crate) fn load_or_create(paths: &AppServerPaths) -> miette::Result<Self> {
        match read_token(&paths.transport_token) {
            Ok(token) => Self::from_token(&token),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let token = std::env::var(APP_SERVER_TRANSPORT_TOKEN_ENV)
                    .unwrap_or_else(|_| generated_token());
                validate_token(&token)?;
                match create_token_file(&paths.transport_token, &token) {
                    Ok(()) => Self::from_token(&token),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        let token = read_token(&paths.transport_token)
                            .map_err(|error| miette::miette!("{error}"))?;
                        Self::from_token(&token)
                    }
                    Err(error) => Err(miette::miette!("{error}")),
                }
            }
            Err(error) => Err(miette::miette!("{error}")),
        }
    }

    pub(crate) fn from_token(token: &str) -> miette::Result<Self> {
        validate_token(token)?;
        Ok(Self {
            token_digest: Sha256::digest(token.as_bytes()).into(),
        })
    }

    pub(crate) fn authorizes(&self, headers: &HeaderMap) -> bool {
        let Some(token) = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
        else {
            return false;
        };
        let candidate: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        constant_time_eq(&self.token_digest, &candidate)
    }
}

fn generated_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn validate_token(token: &str) -> miette::Result<()> {
    if token.len() < 32 || token.len() > 512 || token.chars().any(char::is_whitespace) {
        return Err(miette::miette!(
            "runtime transport token must contain 32..=512 non-whitespace characters"
        ));
    }
    Ok(())
}

fn read_token(path: &Path) -> std::io::Result<String> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_TRANSPORT_TOKEN_BYTES
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "app-server transport token path is not a bounded regular file: {}",
                path.display()
            ),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "app-server transport token must be owner-only: {}",
                    path.display()
                ),
            ));
        }
    }
    let file = File::open(path)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_TRANSPORT_TOKEN_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_TRANSPORT_TOKEN_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "app-server transport token exceeds its size limit",
        ));
    }
    String::from_utf8(bytes)
        .map(|token| token.trim().to_owned())
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

fn create_token_file(path: &Path, token: &str) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(token.as_bytes())?;
    file.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn constant_time_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_authentication_does_not_expose_or_accept_the_wrong_token() {
        let auth = TransportAuth::from_token(&"a".repeat(64)).expect("transport auth");
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {}", "a".repeat(64))
                .parse()
                .expect("authorization"),
        );
        assert!(auth.authorizes(&headers));
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {}", "b".repeat(64))
                .parse()
                .expect("authorization"),
        );
        assert!(!auth.authorizes(&headers));
        assert!(!format!("{auth:?}").contains(&"a".repeat(64)));
    }
}
