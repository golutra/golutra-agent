use std::{
    collections::BTreeMap,
    fmt,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use async_trait::async_trait;
use fs2::FileExt;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::AuthError;

pub const CREDENTIALS_FILE_NAME: &str = "credentials.json";
const CREDENTIALS_LOCK_FILE_NAME: &str = "credentials.lock";
const CREDENTIALS_FILE_VERSION: u32 = 1;
const MAX_CREDENTIALS_FILE_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SecretKind {
    ApiKey,
    Bearer,
    #[serde(rename = "oauth-token-set")]
    OAuthTokenSet,
    StructuredHeaders,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum CredentialSource {
    Environment { key: String },
    Disk,
    Ephemeral,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialRef {
    pub id: String,
    pub source: CredentialSource,
    pub secret_kind: SecretKind,
    pub revision: String,
}

impl CredentialRef {
    #[must_use]
    pub fn disk(secret_kind: SecretKind) -> Self {
        Self::with_source(CredentialSource::Disk, secret_kind)
    }

    #[must_use]
    pub fn ephemeral(secret_kind: SecretKind) -> Self {
        Self::with_source(CredentialSource::Ephemeral, secret_kind)
    }

    pub fn environment(key: impl Into<String>, secret_kind: SecretKind) -> Result<Self, AuthError> {
        let key = key.into();
        validate_environment_key(&key)?;
        Ok(Self::with_source(
            CredentialSource::Environment { key },
            secret_kind,
        ))
    }

    pub fn with_id(
        id: impl Into<String>,
        source: CredentialSource,
        secret_kind: SecretKind,
    ) -> Result<Self, AuthError> {
        let id = id.into();
        validate_identifier(&id, "credential id")?;
        if let CredentialSource::Environment { key } = &source {
            validate_environment_key(key)?;
        }
        Ok(Self {
            id,
            source,
            secret_kind,
            revision: format!("rev_{}", Uuid::now_v7().simple()),
        })
    }

    pub fn validate(&self) -> Result<(), AuthError> {
        validate_identifier(&self.id, "credential id")?;
        validate_identifier(&self.revision, "credential revision")?;
        if let CredentialSource::Environment { key } = &self.source {
            validate_environment_key(key)?;
        }
        Ok(())
    }

    #[must_use]
    pub fn source_label(&self) -> String {
        match &self.source {
            CredentialSource::Environment { key } => format!("env:{key}"),
            CredentialSource::Disk => format!("disk:{}", short_id(&self.id)),
            CredentialSource::Ephemeral => format!("ephemeral:{}", short_id(&self.id)),
        }
    }

    fn with_source(source: CredentialSource, secret_kind: SecretKind) -> Self {
        Self {
            id: format!("cred_{}", Uuid::now_v7().simple()),
            source,
            secret_kind,
            revision: format!("rev_{}", Uuid::now_v7().simple()),
        }
    }
}

pub trait SecretStore: Send + Sync {
    fn get(&self, reference: &CredentialRef) -> Result<Option<SecretString>, AuthError>;
    fn set(&self, reference: &CredentialRef, secret: &SecretString) -> Result<(), AuthError>;
    fn delete(&self, reference: &CredentialRef) -> Result<bool, AuthError>;
    fn health_check(&self) -> Result<(), AuthError>;
}

pub struct DefaultSecretStore {
    home: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct DiskCredentialEntry {
    secret_kind: SecretKind,
    value: String,
}

#[derive(Serialize, Deserialize)]
struct DiskCredentials {
    version: u32,
    credentials: BTreeMap<String, DiskCredentialEntry>,
}

impl Default for DiskCredentials {
    fn default() -> Self {
        Self {
            version: CREDENTIALS_FILE_VERSION,
            credentials: BTreeMap::new(),
        }
    }
}

impl fmt::Debug for DefaultSecretStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DefaultSecretStore")
            .field("home", &self.home)
            .finish_non_exhaustive()
    }
}

impl DefaultSecretStore {
    pub fn new(home: impl Into<PathBuf>) -> Result<Self, AuthError> {
        let home = home.into();
        if home.as_os_str().is_empty() {
            return Err(AuthError::Validation(
                "credential store home cannot be empty".to_owned(),
            ));
        }
        Ok(Self { home })
    }

    #[must_use]
    pub fn credentials_path(&self) -> PathBuf {
        self.home.join(CREDENTIALS_FILE_NAME)
    }

    fn acquire_disk_lock(&self) -> Result<File, AuthError> {
        fs::create_dir_all(&self.home)
            .map_err(|error| secret_store_error("create credential directory", error))?;
        set_owner_only_dir(&self.home)?;
        let path = self.home.join(CREDENTIALS_LOCK_FILE_NAME);
        reject_symlink(&path, "credential lock")?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| secret_store_error("open credential lock", error))?;
        if !file
            .metadata()
            .map_err(|error| secret_store_error("inspect credential lock", error))?
            .is_file()
        {
            return Err(AuthError::SecretStore(
                "credential lock must be a regular file".to_owned(),
            ));
        }
        set_owner_only_file(&path)?;
        file.lock_exclusive()
            .map_err(|error| secret_store_error("lock credential store", error))?;
        Ok(file)
    }

    fn load_disk_credentials(&self) -> Result<DiskCredentials, AuthError> {
        let path = self.credentials_path();
        reject_symlink(&path, "credential file")?;
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(DiskCredentials::default());
            }
            Err(error) => return Err(secret_store_error("open credential file", error)),
        };
        let metadata = file
            .metadata()
            .map_err(|error| secret_store_error("inspect credential file", error))?;
        if !metadata.is_file() {
            return Err(AuthError::SecretStore(
                "credential file must be a regular file".to_owned(),
            ));
        }
        set_owner_only_file(&path)?;
        if metadata.len() > MAX_CREDENTIALS_FILE_BYTES {
            return Err(AuthError::SecretStore(
                "credential file exceeds the size limit".to_owned(),
            ));
        }
        let mut bytes = Vec::new();
        file.take(MAX_CREDENTIALS_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| secret_store_error("read credential file", error))?;
        if bytes.len() as u64 > MAX_CREDENTIALS_FILE_BYTES {
            return Err(AuthError::SecretStore(
                "credential file exceeds the size limit".to_owned(),
            ));
        }
        let credentials: DiskCredentials = serde_json::from_slice(&bytes)
            .map_err(|error| secret_store_error("parse credential file", error))?;
        credentials.validate()?;
        Ok(credentials)
    }

    fn save_disk_credentials(&self, credentials: &DiskCredentials) -> Result<(), AuthError> {
        credentials.validate()?;
        let path = self.credentials_path();
        reject_symlink(&path, "credential file")?;
        if credentials.credentials.is_empty() {
            match fs::remove_file(&path) {
                Ok(()) => sync_directory(&self.home)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(secret_store_error("remove credential file", error)),
            }
            return Ok(());
        }

        let bytes = serde_json::to_vec_pretty(credentials)
            .map_err(|error| secret_store_error("serialize credential file", error))?;
        if bytes.len().saturating_add(1) as u64 > MAX_CREDENTIALS_FILE_BYTES {
            return Err(AuthError::SecretStore(
                "credential file exceeds the size limit".to_owned(),
            ));
        }
        let mut temporary = tempfile::Builder::new()
            .prefix(&format!(".{CREDENTIALS_FILE_NAME}."))
            .suffix(".tmp")
            .tempfile_in(&self.home)
            .map_err(|error| secret_store_error("create temporary credential file", error))?;
        set_owner_only_file(temporary.path())?;
        temporary
            .write_all(&bytes)
            .and_then(|()| temporary.write_all(b"\n"))
            .map_err(|error| secret_store_error("write credential file", error))?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|error| secret_store_error("sync credential file", error))?;
        let persisted = temporary
            .persist(&path)
            .map_err(|error| secret_store_error("replace credential file", error.error))?;
        set_owner_only_file(&path)?;
        persisted
            .sync_all()
            .map_err(|error| secret_store_error("sync credential file", error))?;
        sync_directory(&self.home)
    }

    fn disk_get(&self, reference: &CredentialRef) -> Result<Option<SecretString>, AuthError> {
        let _lock = self.acquire_disk_lock()?;
        let credentials = self.load_disk_credentials()?;
        let Some(entry) = credentials.credentials.get(&reference.id) else {
            return Ok(None);
        };
        if entry.secret_kind != reference.secret_kind {
            return Err(AuthError::SecretStore(format!(
                "credential `{}` kind does not match its reference",
                reference.id
            )));
        }
        Ok(Some(SecretString::from(entry.value.clone())))
    }

    fn disk_set(&self, reference: &CredentialRef, secret: &SecretString) -> Result<(), AuthError> {
        let _lock = self.acquire_disk_lock()?;
        let mut credentials = self.load_disk_credentials()?;
        if credentials
            .credentials
            .get(&reference.id)
            .is_some_and(|entry| entry.secret_kind != reference.secret_kind)
        {
            return Err(AuthError::SecretStore(format!(
                "credential `{}` kind cannot change in place",
                reference.id
            )));
        }
        credentials.credentials.insert(
            reference.id.clone(),
            DiskCredentialEntry {
                secret_kind: reference.secret_kind,
                value: secret.expose_secret().to_owned(),
            },
        );
        self.save_disk_credentials(&credentials)
    }

    fn disk_delete(&self, reference: &CredentialRef) -> Result<bool, AuthError> {
        let _lock = self.acquire_disk_lock()?;
        let mut credentials = self.load_disk_credentials()?;
        let deleted = credentials.credentials.remove(&reference.id).is_some();
        if deleted {
            self.save_disk_credentials(&credentials)?;
        }
        Ok(deleted)
    }

    fn disk_health_check(&self) -> Result<(), AuthError> {
        let _lock = self.acquire_disk_lock()?;
        self.load_disk_credentials().map(|_| ())
    }
}

impl DiskCredentials {
    fn validate(&self) -> Result<(), AuthError> {
        if self.version != CREDENTIALS_FILE_VERSION {
            return Err(AuthError::SecretStore(format!(
                "unsupported credential file version {}",
                self.version
            )));
        }
        for (id, entry) in &self.credentials {
            validate_identifier(id, "credential id")?;
            if entry.value.trim().is_empty() {
                return Err(AuthError::SecretStore(format!(
                    "credential `{id}` has an empty value"
                )));
            }
        }
        Ok(())
    }
}

impl SecretStore for DefaultSecretStore {
    fn get(&self, reference: &CredentialRef) -> Result<Option<SecretString>, AuthError> {
        reference.validate()?;
        match &reference.source {
            CredentialSource::Environment { key } => Ok(std::env::var(key)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .map(SecretString::from)),
            CredentialSource::Disk => self.disk_get(reference),
            CredentialSource::Ephemeral => {
                let account = ephemeral_account(&self.home, &reference.id);
                Ok(ephemeral_secrets()
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .get(&account)
                    .cloned())
            }
        }
    }

    fn set(&self, reference: &CredentialRef, secret: &SecretString) -> Result<(), AuthError> {
        reference.validate()?;
        if secret.expose_secret().trim().is_empty() {
            return Err(AuthError::Validation(
                "credential value cannot be empty".to_owned(),
            ));
        }
        match &reference.source {
            CredentialSource::Environment { .. } => Err(AuthError::Validation(
                "environment credential references are read-only".to_owned(),
            )),
            CredentialSource::Disk => self.disk_set(reference, secret),
            CredentialSource::Ephemeral => {
                let account = ephemeral_account(&self.home, &reference.id);
                ephemeral_secrets()
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(account, secret.clone());
                Ok(())
            }
        }
    }

    fn delete(&self, reference: &CredentialRef) -> Result<bool, AuthError> {
        reference.validate()?;
        match &reference.source {
            CredentialSource::Environment { .. } => Ok(false),
            CredentialSource::Disk => self.disk_delete(reference),
            CredentialSource::Ephemeral => {
                let account = ephemeral_account(&self.home, &reference.id);
                Ok(ephemeral_secrets()
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&account)
                    .is_some())
            }
        }
    }

    fn health_check(&self) -> Result<(), AuthError> {
        self.disk_health_check()
    }
}

#[derive(Debug, Default)]
pub struct MemorySecretStore {
    values: Mutex<BTreeMap<String, SecretString>>,
}

impl MemorySecretStore {
    #[must_use]
    pub fn contains(&self, reference: &CredentialRef) -> bool {
        self.values
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(&reference.id)
    }
}

impl SecretStore for MemorySecretStore {
    fn get(&self, reference: &CredentialRef) -> Result<Option<SecretString>, AuthError> {
        reference.validate()?;
        if let CredentialSource::Environment { key } = &reference.source {
            return Ok(std::env::var(key)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .map(SecretString::from));
        }
        Ok(self
            .values
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&reference.id)
            .cloned())
    }

    fn set(&self, reference: &CredentialRef, secret: &SecretString) -> Result<(), AuthError> {
        reference.validate()?;
        if matches!(reference.source, CredentialSource::Environment { .. }) {
            return Err(AuthError::Validation(
                "environment credential references are read-only".to_owned(),
            ));
        }
        if secret.expose_secret().trim().is_empty() {
            return Err(AuthError::Validation(
                "credential value cannot be empty".to_owned(),
            ));
        }
        self.values
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(reference.id.clone(), secret.clone());
        Ok(())
    }

    fn delete(&self, reference: &CredentialRef) -> Result<bool, AuthError> {
        reference.validate()?;
        if matches!(reference.source, CredentialSource::Environment { .. }) {
            return Ok(false);
        }
        Ok(self
            .values
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&reference.id)
            .is_some())
    }

    fn health_check(&self) -> Result<(), AuthError> {
        Ok(())
    }
}

#[async_trait]
pub trait CredentialProvider: Send + Sync {
    async fn credential(&self, force_refresh: bool) -> Result<SecretString, AuthError>;

    async fn metadata(&self) -> Result<CredentialMetadata, AuthError> {
        Ok(CredentialMetadata::default())
    }

    fn source_label(&self) -> String;
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CredentialMetadata {
    pub account_id: Option<String>,
}

#[derive(Clone)]
pub struct FixedCredentialProvider {
    secret: SecretString,
    source_label: String,
}

impl fmt::Debug for FixedCredentialProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FixedCredentialProvider")
            .field("source_label", &self.source_label)
            .finish_non_exhaustive()
    }
}

impl FixedCredentialProvider {
    #[must_use]
    pub fn new(secret: impl Into<String>, source_label: impl Into<String>) -> Self {
        Self {
            secret: SecretString::from(secret.into()),
            source_label: source_label.into(),
        }
    }
}

#[async_trait]
impl CredentialProvider for FixedCredentialProvider {
    async fn credential(&self, _force_refresh: bool) -> Result<SecretString, AuthError> {
        Ok(self.secret.clone())
    }

    fn source_label(&self) -> String {
        self.source_label.clone()
    }
}

#[derive(Clone)]
pub struct StoredCredentialProvider {
    store: Arc<dyn SecretStore>,
    reference: CredentialRef,
}

impl fmt::Debug for StoredCredentialProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoredCredentialProvider")
            .field("credential_id", &self.reference.id)
            .field("source", &self.reference.source_label())
            .finish_non_exhaustive()
    }
}

impl StoredCredentialProvider {
    #[must_use]
    pub fn new(store: Arc<dyn SecretStore>, reference: CredentialRef) -> Self {
        Self { store, reference }
    }
}

#[async_trait]
impl CredentialProvider for StoredCredentialProvider {
    async fn credential(&self, _force_refresh: bool) -> Result<SecretString, AuthError> {
        let store = Arc::clone(&self.store);
        let reference = self.reference.clone();
        tokio::task::spawn_blocking(move || store.get(&reference))
            .await
            .map_err(|error| AuthError::SecretStore(error.to_string()))??
            .ok_or_else(|| AuthError::SecretNotFound(self.reference.id.clone()))
    }

    fn source_label(&self) -> String {
        self.reference.source_label()
    }
}

fn ephemeral_account(home: &Path, credential_id: &str) -> String {
    let normalized = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
    format!("{}\0{credential_id}", normalized.to_string_lossy())
}

fn validate_identifier(value: &str, label: &str) -> Result<(), AuthError> {
    if value.is_empty()
        || value.len() > 160
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "_-".contains(character))
    {
        return Err(AuthError::Validation(format!("{label} is invalid")));
    }
    Ok(())
}

fn validate_environment_key(value: &str) -> Result<(), AuthError> {
    if value.is_empty()
        || value.len() > 128
        || !value.chars().all(|character| {
            character.is_ascii_uppercase() || character.is_ascii_digit() || character == '_'
        })
        || value.as_bytes()[0].is_ascii_digit()
    {
        return Err(AuthError::Validation(
            "environment credential key is invalid".to_owned(),
        ));
    }
    Ok(())
}

fn short_id(value: &str) -> String {
    let mut suffix = value.chars().rev().take(8).collect::<Vec<_>>();
    suffix.reverse();
    suffix.into_iter().collect()
}

fn ephemeral_secrets() -> &'static Mutex<BTreeMap<String, SecretString>> {
    static SECRETS: OnceLock<Mutex<BTreeMap<String, SecretString>>> = OnceLock::new();
    SECRETS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn reject_symlink(path: &Path, label: &str) -> Result<(), AuthError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(AuthError::SecretStore(format!(
            "{label} must not be a symbolic link"
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(secret_store_error(format!("inspect {label}"), error)),
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), AuthError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| secret_store_error("sync credential directory", error))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), AuthError> {
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_dir(path: &Path) -> Result<(), AuthError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| secret_store_error("secure credential directory", error))
}

#[cfg(not(unix))]
fn set_owner_only_dir(_path: &Path) -> Result<(), AuthError> {
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_file(path: &Path) -> Result<(), AuthError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| secret_store_error("secure credential file", error))
}

#[cfg(not(unix))]
fn set_owner_only_file(_path: &Path) -> Result<(), AuthError> {
    Ok(())
}

fn secret_store_error(action: impl fmt::Display, error: impl fmt::Display) -> AuthError {
    AuthError::SecretStore(format!("{action}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_reference_serialization_never_contains_secret_values() {
        let reference = CredentialRef::disk(SecretKind::ApiKey);
        let value = serde_json::to_string(&reference).expect("serialize");

        assert!(value.contains("disk"));
        assert!(!value.contains("secret-value"));
    }

    #[test]
    fn credential_reference_rejects_path_like_ids_and_invalid_revisions() {
        let path_like = CredentialRef {
            id: "../outside".to_owned(),
            source: CredentialSource::Disk,
            secret_kind: SecretKind::ApiKey,
            revision: "rev_valid".to_owned(),
        };
        let invalid_revision = CredentialRef {
            id: "cred_valid".to_owned(),
            source: CredentialSource::Disk,
            secret_kind: SecretKind::ApiKey,
            revision: "../revision".to_owned(),
        };

        assert!(path_like.validate().is_err());
        assert!(invalid_revision.validate().is_err());
    }

    #[test]
    fn memory_store_roundtrips_and_deletes_secret() {
        let store = MemorySecretStore::default();
        let reference = CredentialRef::disk(SecretKind::ApiKey);
        let secret = SecretString::from("secret-value".to_owned());

        store.set(&reference, &secret).expect("set");
        assert_eq!(
            store
                .get(&reference)
                .expect("get")
                .expect("secret")
                .expose_secret(),
            "secret-value"
        );
        assert!(store.delete(&reference).expect("delete"));
        assert!(store.get(&reference).expect("get after delete").is_none());
    }

    #[test]
    fn default_ephemeral_store_is_scoped_by_golutra_home() {
        let first_home = tempfile::tempdir().expect("first home");
        let second_home = tempfile::tempdir().expect("second home");
        let first = DefaultSecretStore::new(first_home.path()).expect("first store");
        let second = DefaultSecretStore::new(second_home.path()).expect("second store");
        let reference = CredentialRef::with_id(
            "cred_shared_id",
            CredentialSource::Ephemeral,
            SecretKind::ApiKey,
        )
        .expect("reference");

        first
            .set(&reference, &SecretString::from("first-secret".to_owned()))
            .expect("set");

        assert!(second.get(&reference).expect("get second").is_none());
        assert!(first.delete(&reference).expect("delete"));
    }

    #[test]
    fn disk_store_persists_across_instances_without_losing_updates() {
        let home = tempfile::tempdir().expect("home");
        let first = DefaultSecretStore::new(home.path()).expect("first store");
        let second = DefaultSecretStore::new(home.path()).expect("second store");
        let first_reference = CredentialRef::disk(SecretKind::ApiKey);
        let second_reference = CredentialRef::disk(SecretKind::Bearer);

        first
            .set(
                &first_reference,
                &SecretString::from("first-secret".to_owned()),
            )
            .expect("set first");
        second
            .set(
                &second_reference,
                &SecretString::from("second-secret".to_owned()),
            )
            .expect("set second");

        assert_eq!(
            second
                .get(&first_reference)
                .expect("get first")
                .expect("first secret")
                .expose_secret(),
            "first-secret"
        );
        assert_eq!(
            first
                .get(&second_reference)
                .expect("get second")
                .expect("second secret")
                .expose_secret(),
            "second-secret"
        );
    }

    #[test]
    fn deleting_the_last_disk_credential_removes_the_secret_file() {
        let home = tempfile::tempdir().expect("home");
        let store = DefaultSecretStore::new(home.path()).expect("store");
        let reference = CredentialRef::disk(SecretKind::ApiKey);

        store
            .set(&reference, &SecretString::from("secret-value".to_owned()))
            .expect("set");
        assert!(store.credentials_path().is_file());

        assert!(store.delete(&reference).expect("delete"));
        assert!(!store.credentials_path().exists());
    }

    #[test]
    fn disk_store_rejects_corrupt_and_oversized_files() {
        let corrupt_home = tempfile::tempdir().expect("corrupt home");
        let corrupt = DefaultSecretStore::new(corrupt_home.path()).expect("corrupt store");
        fs::write(corrupt.credentials_path(), b"not-json").expect("write corrupt file");
        assert!(corrupt.health_check().is_err());

        let oversized_home = tempfile::tempdir().expect("oversized home");
        let oversized = DefaultSecretStore::new(oversized_home.path()).expect("oversized store");
        let file = File::create(oversized.credentials_path()).expect("create oversized file");
        file.set_len(MAX_CREDENTIALS_FILE_BYTES + 1)
            .expect("resize oversized file");
        assert!(oversized.health_check().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn disk_store_uses_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let parent = tempfile::tempdir().expect("parent");
        let home = parent.path().join("credentials-home");
        let store = DefaultSecretStore::new(&home).expect("store");
        let reference = CredentialRef::disk(SecretKind::ApiKey);

        store
            .set(&reference, &SecretString::from("secret-value".to_owned()))
            .expect("set");

        assert_eq!(
            fs::metadata(&home)
                .expect("home metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(store.credentials_path())
                .expect("credentials metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(home.join(CREDENTIALS_LOCK_FILE_NAME))
                .expect("lock metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn disk_store_rejects_symbolic_link_credential_file() {
        use std::os::unix::fs::symlink;

        let home = tempfile::tempdir().expect("home");
        let target = home.path().join("target.json");
        fs::write(&target, b"{}").expect("target");
        symlink(&target, home.path().join(CREDENTIALS_FILE_NAME)).expect("symlink");
        let store = DefaultSecretStore::new(home.path()).expect("store");

        let error = store.health_check().expect_err("symlink rejected");

        assert!(error.to_string().contains("symbolic link"));
    }
}
