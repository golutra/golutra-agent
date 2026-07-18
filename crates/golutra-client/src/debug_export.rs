//! Local, redacted export of session history and governed runtime facts.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
};

use base64::Engine;
use chrono::{DateTime, Utc};
use golutra_core::{ArtifactId, ArtifactRecord, RedactionStatus, SessionId, TaskId, TraceView};
use golutra_protocol::{
    ArtifactReadRequest, EventPageDirection, EventPageRequest, RuntimeEvent, RuntimeEventType,
    SessionRangeDirection, SessionRangeSpec, SessionSummary, SessionWindowRequest,
    TaskTraceRequest,
};
use golutra_store::MAX_ARTIFACT_READ_BYTES;
use golutra_tools::redact_sensitive_text;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{
    ClientError, RuntimeClient, RuntimeTransport, TaskTraceClient, redact_provider_json,
    set_owner_only_file,
};

const EXPORT_FORMAT_VERSION: u32 = 1;
const MAX_EVENT_EXPORT_PAGES: usize = 65_536;
const MAX_ARTIFACT_EXPORT_BYTES: u64 = 8 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugExportRequest {
    pub selection: SessionWindowRequest,
    pub destination: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebugExportReceipt {
    pub destination: PathBuf,
    pub session_count: usize,
    pub task_count: usize,
    pub artifact_count: usize,
    pub complete: bool,
    pub manifest_checksum: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebugExportManifest {
    pub format: String,
    pub format_version: u32,
    pub generated_at: DateTime<Utc>,
    pub workspace_id: String,
    pub workspace_root: String,
    pub selection: SessionWindowRequest,
    pub mode: String,
    pub complete: bool,
    pub redacted: bool,
    pub redacted_fields: Vec<String>,
    pub missing_data: Vec<String>,
    pub retention_losses: Vec<String>,
    pub sessions: Vec<ExportedSessionManifest>,
    pub artifacts: Vec<ExportedArtifactManifest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportedSessionManifest {
    pub thread_id: String,
    pub session_id: SessionId,
    pub path: String,
    pub event_count: usize,
    pub events_complete: bool,
    pub tasks: Vec<ExportedTaskManifest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportedTaskManifest {
    pub task_id: TaskId,
    pub trace_path: String,
    pub complete: bool,
    pub unresolved_refs: Vec<String>,
    pub missing_sections: Vec<String>,
    pub retention_losses: Vec<String>,
    pub redacted_fields: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportedArtifactState {
    Exported,
    Deduplicated,
    OmittedRaw,
    Missing,
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportedArtifactManifest {
    pub artifact_id: ArtifactId,
    pub checksum: String,
    pub size_bytes: u64,
    pub redaction_status: RedactionStatus,
    pub retention_policy: String,
    pub state: ExportedArtifactState,
    pub path: Option<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ConversationEntry {
    sequence_no: u64,
    timestamp: DateTime<Utc>,
    turn_id: Option<String>,
    task_id: Option<TaskId>,
    role: &'static str,
    content: String,
}

#[derive(Debug, Clone)]
pub struct DebugExportCoordinator<'a> {
    transport: &'a RuntimeTransport,
}

impl<'a> DebugExportCoordinator<'a> {
    #[must_use]
    pub fn new(transport: &'a RuntimeTransport) -> Self {
        Self { transport }
    }

    pub async fn export(
        &self,
        request: DebugExportRequest,
    ) -> Result<DebugExportReceipt, ClientError> {
        let parent = validate_export_destination(&request.destination)?;
        let window = self
            .transport
            .session_window(request.selection.clone())
            .await?;
        if window.sessions.is_empty() {
            return Err(ClientError::InvalidSession(
                "debug export selection contains no sessions".to_owned(),
            ));
        }

        let temporary = tempfile::Builder::new()
            .prefix(".golutra-export-")
            .tempdir_in(&parent)
            .map_err(|error| export_io(&parent, error))?;
        let staging = temporary.path();
        let artifacts_root = staging.join("artifacts/sha256");
        let partial_root = staging.join("artifacts/.partial");
        create_private_dir(&artifacts_root)?;
        create_private_dir(&partial_root)?;

        let mut combined_markdown = String::from("# Golutra conversation export\n\n");
        let mut session_manifests = Vec::with_capacity(window.sessions.len());
        let mut artifact_records = BTreeMap::<ArtifactId, ArtifactRecord>::new();
        let mut missing_data = Vec::new();
        let mut retention_losses = Vec::new();
        let mut redacted_fields = BTreeSet::from([
            "provider_credentials".to_owned(),
            "restricted_context_request".to_owned(),
            "raw_artifact_blobs".to_owned(),
        ]);

        for session in &window.sessions {
            let exported = self
                .export_session(
                    staging,
                    session,
                    &mut combined_markdown,
                    &mut artifact_records,
                    &mut missing_data,
                    &mut retention_losses,
                    &mut redacted_fields,
                )
                .await?;
            session_manifests.push(exported);
        }
        write_bytes(
            &staging.join("conversation.md"),
            combined_markdown.as_bytes(),
        )?;

        let mut artifact_manifests = Vec::with_capacity(artifact_records.len());
        let mut exported_checksums = BTreeMap::<String, String>::new();
        for artifact in artifact_records.into_values() {
            let result = self
                .export_artifact(
                    &artifact,
                    &artifacts_root,
                    &partial_root,
                    &mut exported_checksums,
                )
                .await;
            match &result {
                ExportedArtifactManifest {
                    state: ExportedArtifactState::Missing,
                    detail,
                    ..
                }
                | ExportedArtifactManifest {
                    state: ExportedArtifactState::Invalid,
                    detail,
                    ..
                } => missing_data.push(format!(
                    "artifact:{}:{}",
                    artifact.artifact_id,
                    detail.as_deref().unwrap_or("unavailable")
                )),
                ExportedArtifactManifest {
                    state: ExportedArtifactState::OmittedRaw,
                    ..
                } => {
                    redacted_fields.insert(format!("artifact:{}:raw_blob", artifact.artifact_id));
                }
                _ => {}
            }
            artifact_manifests.push(result);
        }
        fs::remove_dir(&partial_root).map_err(|error| export_io(&partial_root, error))?;

        missing_data.sort();
        missing_data.dedup();
        retention_losses.sort();
        retention_losses.dedup();
        let complete = missing_data.is_empty()
            && retention_losses.is_empty()
            && session_manifests.iter().all(|session| {
                session.events_complete && session.tasks.iter().all(|task| task.complete)
            });
        let manifest = DebugExportManifest {
            format: "golutra-debug-export".to_owned(),
            format_version: EXPORT_FORMAT_VERSION,
            generated_at: Utc::now(),
            workspace_id: self.transport.workspace_id().to_string(),
            workspace_root: self
                .transport
                .cwd()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            selection: request.selection,
            mode: "full-redacted".to_owned(),
            complete,
            redacted: true,
            redacted_fields: redacted_fields.into_iter().collect(),
            missing_data,
            retention_losses,
            sessions: session_manifests,
            artifacts: artifact_manifests,
        };
        let manifest_bytes = redacted_json_bytes(&manifest, true)?;
        write_bytes(&staging.join("manifest.json"), &manifest_bytes)?;
        sync_export_tree(staging)?;

        if fs::symlink_metadata(&request.destination).is_ok() {
            return Err(ClientError::Io(format!(
                "debug export destination already exists: {}",
                request.destination.display()
            )));
        }
        let staging_path = temporary.keep();
        if let Err(error) = fs::rename(&staging_path, &request.destination) {
            let _ = fs::remove_dir_all(&staging_path);
            return Err(export_io(&request.destination, error));
        }
        sync_directory(&parent)?;

        Ok(DebugExportReceipt {
            destination: request.destination,
            session_count: manifest.sessions.len(),
            task_count: manifest
                .sessions
                .iter()
                .map(|session| session.tasks.len())
                .sum(),
            artifact_count: manifest.artifacts.len(),
            complete: manifest.complete,
            manifest_checksum: format!("sha256:{:x}", Sha256::digest(&manifest_bytes)),
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn export_session(
        &self,
        staging: &Path,
        summary: &SessionSummary,
        combined_markdown: &mut String,
        artifact_records: &mut BTreeMap<ArtifactId, ArtifactRecord>,
        missing_data: &mut Vec<String>,
        retention_losses: &mut Vec<String>,
        redacted_fields: &mut BTreeSet<String>,
    ) -> Result<ExportedSessionManifest, ClientError> {
        let relative_root = format!("sessions/{}", summary.session_id);
        let session_root = staging.join(&relative_root);
        create_private_dir(&session_root)?;
        create_private_dir(&session_root.join("tasks"))?;

        let thread = self
            .transport
            .thread_for_session(summary.session_id)
            .await?
            .ok_or_else(|| {
                ClientError::InvalidSession(format!(
                    "thread for session `{}` disappeared during export",
                    summary.session_id
                ))
            })?;
        write_json(&session_root.join("thread.json"), &thread, true)?;

        let events = self.load_all_events(summary.session_id).await?;
        write_json_lines(&session_root.join("events.jsonl"), &events)?;
        let conversation = conversation_entries(&events);
        write_json_lines(&session_root.join("conversation.jsonl"), &conversation)?;
        append_markdown_session(combined_markdown, summary, &conversation);

        let task_ids = events
            .iter()
            .filter_map(|event| event.task_id)
            .collect::<BTreeSet<_>>();
        let mut tasks = Vec::with_capacity(task_ids.len());
        for task_id in task_ids {
            let trace = self
                .transport
                .complete_task_trace(TaskTraceRequest {
                    session_id: summary.session_id,
                    task_id,
                    view: TraceView::Full,
                    cursor: None,
                    limit: 512,
                    wait_for_evaluation: false,
                })
                .await?;
            let task_root = session_root.join("tasks").join(task_id.to_string());
            create_private_dir(&task_root)?;
            write_json(&task_root.join("trace.json"), &trace, true)?;
            for artifact in &trace.artifacts {
                artifact_records
                    .entry(artifact.artifact_id)
                    .or_insert_with(|| artifact.clone());
            }
            missing_data.extend(
                trace
                    .integrity
                    .unresolved_refs
                    .iter()
                    .map(|value| format!("task:{task_id}:{value}")),
            );
            missing_data.extend(
                trace
                    .integrity
                    .missing_sections
                    .iter()
                    .map(|value| format!("task:{task_id}:{value}")),
            );
            retention_losses.extend(
                trace
                    .integrity
                    .retention_losses
                    .iter()
                    .map(|value| format!("task:{task_id}:{value}")),
            );
            redacted_fields.extend(
                trace
                    .integrity
                    .redacted_fields
                    .iter()
                    .map(|value| format!("task:{task_id}:{value}")),
            );
            tasks.push(ExportedTaskManifest {
                task_id,
                trace_path: format!("{relative_root}/tasks/{task_id}/trace.json"),
                complete: trace.integrity.complete,
                unresolved_refs: trace.integrity.unresolved_refs,
                missing_sections: trace.integrity.missing_sections,
                retention_losses: trace.integrity.retention_losses,
                redacted_fields: trace.integrity.redacted_fields,
            });
        }

        Ok(ExportedSessionManifest {
            thread_id: summary.thread_id.to_string(),
            session_id: summary.session_id,
            path: relative_root,
            event_count: events.len(),
            events_complete: true,
            tasks,
        })
    }

    async fn load_all_events(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<RuntimeEvent>, ClientError> {
        let mut cursor = None;
        let mut events = Vec::new();
        for _ in 0..MAX_EVENT_EXPORT_PAGES {
            let page = self
                .transport
                .event_page(EventPageRequest {
                    session_id,
                    task_id: None,
                    cursor,
                    direction: EventPageDirection::Forward,
                    limit: 512,
                })
                .await?;
            if page.events.is_empty() {
                if page.has_more {
                    return Err(ClientError::TaskExecution(
                        "event export page has_more without events".to_owned(),
                    ));
                }
                return Ok(events);
            }
            let next = page.end_cursor.ok_or_else(|| {
                ClientError::TaskExecution("event export page has no end cursor".to_owned())
            })?;
            if cursor == Some(next) {
                return Err(ClientError::TaskExecution(
                    "event export cursor did not advance".to_owned(),
                ));
            }
            cursor = Some(next);
            events.extend(page.events);
            if !page.has_more {
                return Ok(events);
            }
        }
        Err(ClientError::TaskExecution(format!(
            "session event export exceeds {MAX_EVENT_EXPORT_PAGES} pages"
        )))
    }

    async fn export_artifact(
        &self,
        artifact: &ArtifactRecord,
        artifacts_root: &Path,
        partial_root: &Path,
        exported_checksums: &mut BTreeMap<String, String>,
    ) -> ExportedArtifactManifest {
        let mut manifest = ExportedArtifactManifest {
            artifact_id: artifact.artifact_id,
            checksum: artifact.checksum.clone(),
            size_bytes: artifact.size_bytes,
            redaction_status: artifact.redaction_status,
            retention_policy: artifact.retention_policy.clone(),
            state: ExportedArtifactState::Missing,
            path: None,
            detail: None,
        };
        if artifact.redaction_status == RedactionStatus::Raw {
            manifest.state = ExportedArtifactState::OmittedRaw;
            manifest.detail =
                Some("raw artifact blobs are excluded from full-redacted exports".to_owned());
            return manifest;
        }
        let Some(checksum_hex) = sha256_checksum_hex(&artifact.checksum) else {
            manifest.state = ExportedArtifactState::Invalid;
            manifest.detail = Some("artifact checksum is not a valid sha256 digest".to_owned());
            return manifest;
        };
        let relative_path = format!("artifacts/sha256/{checksum_hex}");
        if let Some(existing) = exported_checksums.get(&artifact.checksum) {
            manifest.state = ExportedArtifactState::Deduplicated;
            manifest.path = Some(existing.clone());
            return manifest;
        }
        if artifact.size_bytes > MAX_ARTIFACT_EXPORT_BYTES {
            manifest.state = ExportedArtifactState::Invalid;
            manifest.detail = Some(format!(
                "artifact exceeds export limit of {MAX_ARTIFACT_EXPORT_BYTES} bytes"
            ));
            return manifest;
        }

        let partial_path = partial_root.join(artifact.artifact_id.to_string());
        let mut file = match File::options()
            .write(true)
            .create_new(true)
            .open(&partial_path)
        {
            Ok(file) => file,
            Err(error) => {
                manifest.detail = Some(format!("cannot create partial artifact: {error}"));
                return manifest;
            }
        };
        if let Err(error) = set_owner_only_file(&partial_path) {
            let _ = fs::remove_file(&partial_path);
            manifest.detail = Some(error.to_string());
            return manifest;
        }

        let mut offset = 0_u64;
        let mut hasher = Sha256::new();
        while offset < artifact.size_bytes {
            let length = (artifact.size_bytes - offset).min(MAX_ARTIFACT_READ_BYTES);
            let chunk = match self
                .transport
                .read_artifact_chunk(ArtifactReadRequest {
                    artifact_id: artifact.artifact_id,
                    offset,
                    length,
                })
                .await
            {
                Ok(Some(chunk)) => chunk,
                Ok(None) => {
                    manifest.detail =
                        Some(format!("artifact blob is unavailable at offset {offset}"));
                    let _ = fs::remove_file(&partial_path);
                    return manifest;
                }
                Err(error) => {
                    manifest.detail = Some(format!("artifact read failed: {error}"));
                    let _ = fs::remove_file(&partial_path);
                    return manifest;
                }
            };
            let bytes =
                match base64::engine::general_purpose::STANDARD.decode(&chunk.content_base64) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        manifest.state = ExportedArtifactState::Invalid;
                        manifest.detail = Some(format!("artifact chunk is not base64: {error}"));
                        let _ = fs::remove_file(&partial_path);
                        return manifest;
                    }
                };
            let bytes_len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            if chunk.artifact_id != artifact.artifact_id
                || chunk.offset != offset
                || chunk.length != bytes_len
                || chunk.total_size != artifact.size_bytes
                || chunk.checksum != artifact.checksum
                || bytes.is_empty()
            {
                manifest.state = ExportedArtifactState::Invalid;
                manifest.detail = Some("artifact chunk metadata is inconsistent".to_owned());
                let _ = fs::remove_file(&partial_path);
                return manifest;
            }
            if let Err(error) = file.write_all(&bytes) {
                manifest.detail = Some(format!("artifact write failed: {error}"));
                let _ = fs::remove_file(&partial_path);
                return manifest;
            }
            hasher.update(&bytes);
            offset = offset.saturating_add(bytes_len);
            if chunk.eof && offset != artifact.size_bytes {
                manifest.state = ExportedArtifactState::Invalid;
                manifest.detail = Some("artifact ended before its declared size".to_owned());
                let _ = fs::remove_file(&partial_path);
                return manifest;
            }
        }
        if let Err(error) = file.sync_all() {
            manifest.detail = Some(format!("artifact sync failed: {error}"));
            let _ = fs::remove_file(&partial_path);
            return manifest;
        }
        drop(file);
        let actual = format!("{:x}", hasher.finalize());
        if actual != checksum_hex {
            manifest.state = ExportedArtifactState::Invalid;
            manifest.detail = Some("artifact checksum mismatch".to_owned());
            let _ = fs::remove_file(&partial_path);
            return manifest;
        }
        let final_path = artifacts_root.join(&checksum_hex);
        if let Err(error) = fs::rename(&partial_path, &final_path) {
            manifest.detail = Some(format!("artifact finalize failed: {error}"));
            let _ = fs::remove_file(&partial_path);
            return manifest;
        }
        if let Err(error) = set_owner_only_file(&final_path) {
            manifest.detail = Some(error.to_string());
            let _ = fs::remove_file(&final_path);
            return manifest;
        }
        exported_checksums.insert(artifact.checksum.clone(), relative_path.clone());
        manifest.state = ExportedArtifactState::Exported;
        manifest.path = Some(relative_path);
        manifest
    }
}

pub fn parse_session_range(input: &str) -> Result<SessionRangeSpec, ClientError> {
    let input = input.trim();
    if input.is_empty() || input == "1" {
        return Ok(SessionRangeSpec {
            direction: SessionRangeDirection::Single,
            count: 1,
        });
    }
    let (direction, count) = match input.as_bytes().first() {
        Some(b'+') => (SessionRangeDirection::Newer, &input[1..]),
        Some(b'-') => (SessionRangeDirection::Older, &input[1..]),
        _ => {
            return Err(ClientError::InvalidSession(
                "session export range must be `1`, `+N`, or `-N`".to_owned(),
            ));
        }
    };
    let count = count.parse::<u32>().map_err(|_| {
        ClientError::InvalidSession("session export range count is invalid".to_owned())
    })?;
    if count == 0 || count > 500 {
        return Err(ClientError::InvalidSession(
            "session export range count must be between 1 and 500".to_owned(),
        ));
    }
    Ok(SessionRangeSpec { direction, count })
}

fn validate_export_destination(destination: &Path) -> Result<PathBuf, ClientError> {
    if !destination.is_absolute() {
        return Err(ClientError::Io(format!(
            "debug export destination must be absolute: {}",
            destination.display()
        )));
    }
    if destination.file_name().is_none() {
        return Err(ClientError::Io(
            "debug export destination must name a new directory".to_owned(),
        ));
    }
    match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(ClientError::Io(format!(
                "debug export destination cannot be a symbolic link: {}",
                destination.display()
            )));
        }
        Ok(_) => {
            return Err(ClientError::Io(format!(
                "debug export destination already exists: {}",
                destination.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(export_io(destination, error)),
    }
    let parent = destination.parent().ok_or_else(|| {
        ClientError::Io(format!(
            "debug export destination has no parent: {}",
            destination.display()
        ))
    })?;
    match fs::symlink_metadata(parent) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(ClientError::Io(format!(
            "debug export parent cannot be a symbolic link: {}",
            parent.display()
        ))),
        Ok(metadata) if metadata.is_dir() => Ok(parent.to_path_buf()),
        Ok(_) => Err(ClientError::Io(format!(
            "debug export parent is not a directory: {}",
            parent.display()
        ))),
        Err(error) => Err(export_io(parent, error)),
    }
}

fn create_private_dir(path: &Path) -> Result<(), ClientError> {
    fs::create_dir_all(path).map_err(|error| export_io(path, error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| export_io(path, error))?;
    }
    Ok(())
}

fn write_json<T: Serialize>(path: &Path, value: &T, redact: bool) -> Result<(), ClientError> {
    let bytes = if redact {
        redacted_json_bytes(value, true)?
    } else {
        serde_json::to_vec_pretty(value)?
    };
    write_bytes(path, &bytes)
}

fn write_json_lines<T: Serialize>(path: &Path, values: &[T]) -> Result<(), ClientError> {
    let mut bytes = Vec::new();
    for value in values {
        let mut json = serde_json::to_value(value)?;
        redact_provider_json(&mut json);
        serde_json::to_writer(&mut bytes, &json)?;
        bytes.push(b'\n');
    }
    write_bytes(path, &bytes)
}

fn redacted_json_bytes<T: Serialize>(value: &T, pretty: bool) -> Result<Vec<u8>, ClientError> {
    let mut value = serde_json::to_value(value)?;
    redact_provider_json(&mut value);
    if pretty {
        serde_json::to_vec_pretty(&value).map_err(ClientError::Serialization)
    } else {
        serde_json::to_vec(&value).map_err(ClientError::Serialization)
    }
}

fn write_bytes(path: &Path, bytes: &[u8]) -> Result<(), ClientError> {
    if fs::symlink_metadata(path).is_ok() {
        return Err(ClientError::Io(format!(
            "debug export file already exists: {}",
            path.display()
        )));
    }
    let mut file = File::options()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| export_io(path, error))?;
    set_owner_only_file(path)?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| export_io(path, error))
}

fn conversation_entries(events: &[RuntimeEvent]) -> Vec<ConversationEntry> {
    let mut entries = Vec::new();
    let mut user_turns = HashSet::new();
    for event in events {
        let (role, content) = match event.event_type {
            RuntimeEventType::TaskCreated | RuntimeEventType::TurnQueued => (
                "user",
                event
                    .payload
                    .pointer("/payload/prompt")
                    .and_then(Value::as_str),
            ),
            RuntimeEventType::TurnStarted => {
                ("user", event.payload.get("prompt").and_then(Value::as_str))
            }
            RuntimeEventType::AssistantMessage => (
                "assistant",
                event.payload.get("content").and_then(Value::as_str),
            ),
            _ => continue,
        };
        let Some(content) = content.filter(|content| !content.trim().is_empty()) else {
            continue;
        };
        if role == "user"
            && let Some(turn_id) = event.turn_id
            && !user_turns.insert(turn_id)
        {
            continue;
        }
        let content = redact_sensitive_text(content).0;
        entries.push(ConversationEntry {
            sequence_no: event.sequence_no,
            timestamp: event.timestamp,
            turn_id: event.turn_id.map(|turn_id| turn_id.to_string()),
            task_id: event.task_id,
            role,
            content,
        });
    }
    entries
}

fn append_markdown_session(
    output: &mut String,
    session: &SessionSummary,
    entries: &[ConversationEntry],
) {
    output.push_str(&format!(
        "## {}\n\nSession: `{}`  \nThread: `{}`\n\n",
        session.title, session.session_id, session.thread_id
    ));
    for entry in entries {
        output.push_str(if entry.role == "user" {
            "### You\n\n"
        } else {
            "### Golutra\n\n"
        });
        output.push_str(&entry.content);
        output.push_str("\n\n");
    }
}

fn sha256_checksum_hex(checksum: &str) -> Option<String> {
    let value = checksum.strip_prefix("sha256:")?.to_ascii_lowercase();
    (value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(value)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), ClientError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| export_io(path, error))
}

#[cfg(unix)]
fn sync_export_tree(root: &Path) -> Result<(), ClientError> {
    let mut directories = walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| ClientError::Io(format!("{}: {error}", root.display())))?
        .into_iter()
        .filter(|entry| entry.file_type().is_dir())
        .map(|entry| entry.into_path())
        .collect::<Vec<_>>();
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for directory in directories {
        sync_directory(&directory)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), ClientError> {
    Ok(())
}

#[cfg(not(unix))]
fn sync_export_tree(_root: &Path) -> Result<(), ClientError> {
    Ok(())
}

fn export_io(path: &Path, error: std::io::Error) -> ClientError {
    ClientError::Io(format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_range_syntax_matches_anchor_selection_contract() {
        assert_eq!(
            parse_session_range("").expect("single"),
            SessionRangeSpec {
                direction: SessionRangeDirection::Single,
                count: 1,
            }
        );
        assert_eq!(
            parse_session_range("+50").expect("newer"),
            SessionRangeSpec {
                direction: SessionRangeDirection::Newer,
                count: 50,
            }
        );
        assert_eq!(
            parse_session_range("-50").expect("older"),
            SessionRangeSpec {
                direction: SessionRangeDirection::Older,
                count: 50,
            }
        );
        for invalid in ["0", "50", "+0", "-501", "+nope"] {
            assert!(parse_session_range(invalid).is_err(), "{invalid}");
        }
    }
}
