use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
};

use chrono::{DateTime, Utc};
use fs2::FileExt;
use golutra_core::{EvidenceId, MemoryId, TaskId};
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const MAX_MEMORY_CONTENT_BYTES: usize = 64 * 1024;
const MAX_MEMORY_STATE_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkingSummary {
    pub objective: String,
    pub completion_criteria: Vec<String>,
    pub done: Vec<String>,
    pub in_progress: Vec<String>,
    pub blocked: Vec<String>,
    pub key_files: Vec<String>,
    pub key_evidence: Vec<EvidenceId>,
    pub unresolved_items: Vec<String>,
    pub next_steps: Vec<String>,
    pub risks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MemoryCandidate {
    pub source_task_id: TaskId,
    pub evidence_ids: Vec<EvidenceId>,
    pub proposed_scope: String,
    pub confidence: u8,
    pub contradiction_ids: Vec<String>,
    pub expiry: Option<String>,
    pub promotion_status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ContextProjectionCacheEntry {
    pub cache_key: String,
    pub task_id: TaskId,
    pub token_count: u64,
    pub invalidation_refs: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    Active,
    RolledBack,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MemoryRecord {
    pub memory_id: MemoryId,
    pub content: String,
    pub scope: String,
    pub confidence: u8,
    pub source_task_id: TaskId,
    pub evidence_ids: Vec<EvidenceId>,
    pub contradiction_ids: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub version: u64,
    pub status: MemoryStatus,
    pub rollback_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RetrievedMemory {
    pub record: MemoryRecord,
    pub relevance_score: u32,
    pub matched_terms: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MemoryPromotionDecisionKind {
    Approve,
    Reject,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MemoryPromotionDecision {
    pub decision: MemoryPromotionDecisionKind,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryPromotionGate {
    minimum_confidence: u8,
}

impl MemoryPromotionGate {
    #[must_use]
    pub fn new(minimum_confidence: u8) -> Self {
        Self { minimum_confidence }
    }

    #[must_use]
    pub fn evaluate(&self, candidate: &MemoryCandidate, content: &str) -> MemoryPromotionDecision {
        let rejection = if candidate.proposed_scope != "project" {
            Some("only project-scoped memory can be promoted automatically")
        } else if candidate.evidence_ids.is_empty() {
            Some("memory candidate has no durable evidence")
        } else if candidate.confidence < self.minimum_confidence {
            Some("memory candidate confidence is below the promotion threshold")
        } else if !candidate.contradiction_ids.is_empty() {
            Some("memory candidate has unresolved contradictions")
        } else if content.trim().is_empty() {
            Some("memory candidate content is empty")
        } else if content.len() > MAX_MEMORY_CONTENT_BYTES {
            Some("memory candidate content exceeds the promotion limit")
        } else if contains_secret(content) {
            Some("memory candidate may contain a secret")
        } else {
            None
        };
        match rejection {
            Some(reason) => MemoryPromotionDecision {
                decision: MemoryPromotionDecisionKind::Reject,
                reason: reason.to_owned(),
            },
            None => MemoryPromotionDecision {
                decision: MemoryPromotionDecisionKind::Approve,
                reason: "evidence-backed project memory passed the promotion gate".to_owned(),
            },
        }
    }
}

impl Default for MemoryPromotionGate {
    fn default() -> Self {
        Self::new(75)
    }
}

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("memory store IO failed: {0}")]
    Io(String),
    #[error("memory store JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("memory store lock is poisoned")]
    LockPoisoned,
    #[error("memory promotion rejected: {0}")]
    PromotionRejected(String),
    #[error("memory duplicates active record: {0}")]
    Duplicate(MemoryId),
    #[error("memory record not found: {0}")]
    NotFound(MemoryId),
    #[error("memory store limit exceeded: {0}")]
    Limit(String),
}

#[derive(Debug, Clone)]
pub struct MemoryStore {
    path: Option<PathBuf>,
    lock: Arc<Mutex<()>>,
    in_memory_records: Arc<Mutex<Vec<MemoryRecord>>>,
}

impl MemoryStore {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Some(path.into()),
            lock: Arc::new(Mutex::new(())),
            in_memory_records: Arc::new(Mutex::new(Vec::new())),
        }
    }

    #[must_use]
    pub fn in_memory() -> Self {
        Self {
            path: None,
            lock: Arc::new(Mutex::new(())),
            in_memory_records: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn list(&self) -> Result<Vec<MemoryRecord>, MemoryError> {
        let _guard = self.lock.lock().map_err(|_| MemoryError::LockPoisoned)?;
        let _file_lock = self.acquire_file_lock()?;
        self.load_unlocked()
    }

    pub fn promote(
        &self,
        gate: &MemoryPromotionGate,
        candidate: &MemoryCandidate,
        content: impl Into<String>,
    ) -> Result<MemoryRecord, MemoryError> {
        let content = content.into();
        let _guard = self.lock.lock().map_err(|_| MemoryError::LockPoisoned)?;
        let _file_lock = self.acquire_file_lock()?;
        let mut records = self.load_unlocked()?;
        let now = Utc::now();
        let mut checked_candidate = candidate.clone();
        checked_candidate
            .contradiction_ids
            .extend(contradiction_ids_from_records(
                &records,
                &content,
                &candidate.proposed_scope,
            ));
        checked_candidate.contradiction_ids.sort();
        checked_candidate.contradiction_ids.dedup();
        let decision = gate.evaluate(&checked_candidate, &content);
        if decision.decision != MemoryPromotionDecisionKind::Approve {
            return Err(MemoryError::PromotionRejected(decision.reason));
        }
        if let Some(existing) = records.iter().find(|record| {
            record.status == MemoryStatus::Active
                && record.scope == candidate.proposed_scope
                && record.expires_at.is_none_or(|expiry| expiry > now)
                && normalize_content(&record.content) == normalize_content(&content)
        }) {
            return Err(MemoryError::Duplicate(existing.memory_id));
        }
        let expires_at = candidate
            .expiry
            .as_deref()
            .map(DateTime::parse_from_rfc3339)
            .transpose()
            .map_err(|error| MemoryError::Io(error.to_string()))?
            .map(|value| value.with_timezone(&Utc));
        let record = MemoryRecord {
            memory_id: MemoryId::new(),
            content,
            scope: candidate.proposed_scope.clone(),
            confidence: candidate.confidence,
            source_task_id: candidate.source_task_id,
            evidence_ids: candidate.evidence_ids.clone(),
            contradiction_ids: checked_candidate.contradiction_ids,
            created_at: Utc::now(),
            expires_at,
            version: records
                .iter()
                .map(|record| record.version)
                .max()
                .unwrap_or_default()
                .saturating_add(1),
            status: MemoryStatus::Active,
            rollback_reason: None,
        };
        records.push(record.clone());
        self.save_unlocked(&records)?;
        Ok(record)
    }

    pub fn retrieve(
        &self,
        query: &str,
        scope: &str,
        limit: usize,
    ) -> Result<Vec<RetrievedMemory>, MemoryError> {
        let query_terms = terms(query);
        if query_terms.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let now = Utc::now();
        let mut retrieved = self
            .list()?
            .into_iter()
            .filter(|record| record.status == MemoryStatus::Active)
            .filter(|record| record.scope == scope)
            .filter(|record| record.expires_at.is_none_or(|expiry| expiry > now))
            .filter_map(|record| {
                let content_terms = terms(&record.content);
                let mut matched_terms = query_terms
                    .intersection(&content_terms)
                    .cloned()
                    .collect::<Vec<_>>();
                matched_terms.sort();
                (!matched_terms.is_empty()).then_some(RetrievedMemory {
                    relevance_score: u32::try_from(matched_terms.len()).unwrap_or(u32::MAX)
                        * u32::from(record.confidence),
                    matched_terms,
                    record,
                })
            })
            .collect::<Vec<_>>();
        retrieved.sort_by(|left, right| {
            right
                .relevance_score
                .cmp(&left.relevance_score)
                .then_with(|| right.record.created_at.cmp(&left.record.created_at))
        });
        retrieved.truncate(limit);
        Ok(retrieved)
    }

    pub fn contradiction_ids(
        &self,
        content: &str,
        scope: &str,
    ) -> Result<Vec<String>, MemoryError> {
        Ok(contradiction_ids_from_records(
            &self.list()?,
            content,
            scope,
        ))
    }

    pub fn rollback(
        &self,
        memory_id: MemoryId,
        reason: impl Into<String>,
    ) -> Result<MemoryRecord, MemoryError> {
        let _guard = self.lock.lock().map_err(|_| MemoryError::LockPoisoned)?;
        let _file_lock = self.acquire_file_lock()?;
        let mut records = self.load_unlocked()?;
        let record = records
            .iter_mut()
            .find(|record| record.memory_id == memory_id)
            .ok_or(MemoryError::NotFound(memory_id))?;
        record.status = MemoryStatus::RolledBack;
        record.rollback_reason = Some(reason.into());
        let rolled_back = record.clone();
        self.save_unlocked(&records)?;
        Ok(rolled_back)
    }

    fn acquire_file_lock(&self) -> Result<Option<File>, MemoryError> {
        let Some(path) = &self.path else {
            return Ok(None);
        };
        let parent = path.parent().ok_or_else(|| {
            MemoryError::Io(format!("memory path has no parent: {}", path.display()))
        })?;
        fs::create_dir_all(parent).map_err(|error| MemoryError::Io(error.to_string()))?;
        set_owner_only_memory_dir(parent)?;
        let lock_path = path.with_extension("lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|error| MemoryError::Io(error.to_string()))?;
        set_owner_only_memory_file(&lock_path)?;
        file.lock_exclusive()
            .map_err(|error| MemoryError::Io(error.to_string()))?;
        Ok(Some(file))
    }

    fn load_unlocked(&self) -> Result<Vec<MemoryRecord>, MemoryError> {
        let Some(path) = &self.path else {
            return self
                .in_memory_records
                .lock()
                .map(|records| records.clone())
                .map_err(|_| MemoryError::LockPoisoned);
        };
        match read_bounded_memory_file(path)? {
            Some(bytes) => serde_json::from_slice(&bytes).map_err(MemoryError::from),
            None => Ok(Vec::new()),
        }
    }

    fn save_unlocked(&self, records: &[MemoryRecord]) -> Result<(), MemoryError> {
        let encoded = serde_json::to_vec_pretty(records)?;
        if u64::try_from(encoded.len()).unwrap_or(u64::MAX) > MAX_MEMORY_STATE_BYTES {
            return Err(MemoryError::Limit(format!(
                "serialized state exceeds {MAX_MEMORY_STATE_BYTES} bytes"
            )));
        }
        let Some(path) = &self.path else {
            *self
                .in_memory_records
                .lock()
                .map_err(|_| MemoryError::LockPoisoned)? = records.to_vec();
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| MemoryError::Io(error.to_string()))?;
            set_owner_only_memory_dir(parent)?;
        }
        let temporary = temporary_path(path);
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| MemoryError::Io(error.to_string()))?;
        file.write_all(&encoded)
            .map_err(|error| MemoryError::Io(error.to_string()))?;
        file.sync_all()
            .map_err(|error| MemoryError::Io(error.to_string()))?;
        set_owner_only_memory_file(&temporary)?;
        fs::rename(&temporary, path).map_err(|error| MemoryError::Io(error.to_string()))?;
        set_owner_only_memory_file(path)?;
        File::open(path)
            .and_then(|file| file.sync_all())
            .map_err(|error| MemoryError::Io(error.to_string()))?;
        if let Some(parent) = path.parent() {
            sync_memory_directory(parent)?;
        }
        Ok(())
    }
}

fn read_bounded_memory_file(path: &Path) -> Result<Option<Vec<u8>>, MemoryError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(MemoryError::Io(error.to_string())),
    };
    let metadata = file
        .metadata()
        .map_err(|error| MemoryError::Io(error.to_string()))?;
    if metadata.len() > MAX_MEMORY_STATE_BYTES {
        return Err(MemoryError::Limit(format!(
            "{} exceeds {MAX_MEMORY_STATE_BYTES} bytes",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    file.take(MAX_MEMORY_STATE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| MemoryError::Io(error.to_string()))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_MEMORY_STATE_BYTES {
        return Err(MemoryError::Limit(format!(
            "{} grew beyond {MAX_MEMORY_STATE_BYTES} bytes while reading",
            path.display()
        )));
    }
    Ok(Some(bytes))
}

#[must_use]
pub fn propose_project_memory(
    source_task_id: TaskId,
    evidence_ids: Vec<EvidenceId>,
) -> MemoryCandidate {
    MemoryCandidate {
        source_task_id,
        evidence_ids,
        proposed_scope: "project".to_owned(),
        confidence: 80,
        contradiction_ids: Vec::new(),
        expiry: None,
        promotion_status: "proposed".to_owned(),
    }
}

fn temporary_path(path: &Path) -> PathBuf {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| format!("{extension}.tmp"))
        .unwrap_or_else(|| "tmp".to_owned());
    path.with_extension(extension)
}

#[cfg(unix)]
fn sync_memory_directory(path: &Path) -> Result<(), MemoryError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| MemoryError::Io(error.to_string()))
}

#[cfg(not(unix))]
fn sync_memory_directory(_path: &Path) -> Result<(), MemoryError> {
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_memory_dir(path: &Path) -> Result<(), MemoryError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| MemoryError::Io(error.to_string()))
}

#[cfg(not(unix))]
fn set_owner_only_memory_dir(_path: &Path) -> Result<(), MemoryError> {
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_memory_file(path: &Path) -> Result<(), MemoryError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| MemoryError::Io(error.to_string()))
}

#[cfg(not(unix))]
fn set_owner_only_memory_file(_path: &Path) -> Result<(), MemoryError> {
    Ok(())
}

fn terms(value: &str) -> HashSet<String> {
    value
        .split(|character: char| {
            !character.is_alphanumeric() && character != '_' && character != '-'
        })
        .map(str::to_lowercase)
        .filter(|term| term.chars().count() >= 2)
        .collect()
}

fn normalize_content(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn contradiction_ids_from_records(
    records: &[MemoryRecord],
    content: &str,
    scope: &str,
) -> Vec<String> {
    let now = Utc::now();
    let candidate_terms = terms(content);
    if candidate_terms.is_empty() {
        return Vec::new();
    }
    let candidate_content = normalize_content(content);
    records
        .iter()
        .filter(|record| {
            record.status == MemoryStatus::Active
                && record.scope == scope
                && record.expires_at.is_none_or(|expiry| expiry > now)
        })
        .filter(|record| normalize_content(&record.content) != candidate_content)
        .filter(|record| {
            let existing_terms = terms(&record.content);
            let intersection = candidate_terms.intersection(&existing_terms).count();
            let union = candidate_terms.union(&existing_terms).count();
            union > 0 && intersection.saturating_mul(100) / union >= 80
        })
        .map(|record| record.memory_id.to_string())
        .collect()
}

fn contains_secret(value: &str) -> bool {
    static LABELED_SECRET: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?i)(?:api[_-]?key|access[_-]?token|token|secret|password|authorization)["']?\s*[:=]\s*["']?(?:bearer\s+)?[^\s,;"']{8,}"#,
        )
        .expect("memory secret regex is valid")
    });
    let lower = value.to_ascii_lowercase();
    LABELED_SECRET.is_match(value)
        || ["sk-", "ghp_", "github_pat_", "xoxb-", "xoxp-"]
            .iter()
            .any(|prefix| {
                lower.match_indices(prefix).any(|(start, _)| {
                    value[start..]
                        .chars()
                        .take_while(|character| {
                            character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
                        })
                        .count()
                        >= 12
                })
            })
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn promotes_retrieves_and_rolls_back_project_memory() {
        let directory = tempdir().expect("directory");
        let store = MemoryStore::new(directory.path().join("memory.json"));
        let candidate = propose_project_memory(TaskId::new(), vec![EvidenceId::new()]);

        let promoted = store
            .promote(
                &MemoryPromotionGate::default(),
                &candidate,
                "cargo test validates the runtime store",
            )
            .expect("promotion");
        let retrieved = store
            .retrieve("validate runtime with cargo test", "project", 3)
            .expect("retrieval");
        let rolled_back = store
            .rollback(promoted.memory_id, "superseded")
            .expect("rollback");

        assert_eq!(retrieved.len(), 1);
        assert_eq!(retrieved[0].record.memory_id, promoted.memory_id);
        assert_eq!(rolled_back.status, MemoryStatus::RolledBack);
        assert!(
            store
                .retrieve("cargo test runtime", "project", 3)
                .expect("retrieval after rollback")
                .is_empty()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = fs::metadata(directory.path().join("memory.json"))
                .expect("memory file")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn promotion_gate_rejects_unsupported_or_unsafe_candidates() {
        let mut candidate = propose_project_memory(TaskId::new(), Vec::new());
        let gate = MemoryPromotionGate::default();

        assert_eq!(
            gate.evaluate(&candidate, "use cargo test").decision,
            MemoryPromotionDecisionKind::Reject
        );
        candidate.evidence_ids.push(EvidenceId::new());
        assert_eq!(
            gate.evaluate(&candidate, "token sk-1234567890123456")
                .decision,
            MemoryPromotionDecisionKind::Reject
        );
        assert_eq!(
            gate.evaluate(&candidate, "url?key=sk-1234567890123456")
                .decision,
            MemoryPromotionDecisionKind::Reject
        );
        assert_eq!(
            gate.evaluate(&candidate, "API_KEY=plain-secret-value")
                .decision,
            MemoryPromotionDecisionKind::Reject
        );
        assert_eq!(
            gate.evaluate(&candidate, "PASSWORD=p@ssw0rd!").decision,
            MemoryPromotionDecisionKind::Reject
        );
        assert_eq!(
            gate.evaluate(&candidate, &"x".repeat(MAX_MEMORY_CONTENT_BYTES + 1))
                .decision,
            MemoryPromotionDecisionKind::Reject
        );
    }

    #[test]
    fn oversized_memory_state_is_rejected_before_deserialization() {
        let directory = tempdir().expect("directory");
        let path = directory.path().join("memory.json");
        let file = fs::File::create(&path).expect("state file");
        file.set_len(MAX_MEMORY_STATE_BYTES + 1)
            .expect("oversized fixture");
        let store = MemoryStore::new(path);

        assert!(matches!(store.list(), Err(MemoryError::Limit(_))));
    }

    #[test]
    fn in_memory_store_shares_records_between_clones() {
        let store = MemoryStore::in_memory();
        let cloned = store.clone();
        cloned
            .promote(
                &MemoryPromotionGate::default(),
                &propose_project_memory(TaskId::new(), vec![EvidenceId::new()]),
                "workspace tests use cargo test",
            )
            .expect("promotion");

        assert_eq!(store.list().expect("list").len(), 1);
    }

    #[test]
    fn file_backed_store_preserves_updates_from_independent_instances() {
        let directory = tempdir().expect("directory");
        let path = directory.path().join("memory.json");
        let first = MemoryStore::new(&path);
        let second = MemoryStore::new(&path);

        first
            .promote(
                &MemoryPromotionGate::default(),
                &propose_project_memory(TaskId::new(), vec![EvidenceId::new()]),
                "first process validates cargo tests",
            )
            .expect("first promotion");
        second
            .promote(
                &MemoryPromotionGate::default(),
                &propose_project_memory(TaskId::new(), vec![EvidenceId::new()]),
                "second process validates runtime events",
            )
            .expect("second promotion");

        assert_eq!(first.list().expect("shared records").len(), 2);
        assert!(path.with_extension("lock").exists());
    }

    #[test]
    fn contradiction_check_flags_near_duplicate_facts() {
        let store = MemoryStore::in_memory();
        store
            .promote(
                &MemoryPromotionGate::default(),
                &propose_project_memory(TaskId::new(), vec![EvidenceId::new()]),
                "runtime tests use cargo test and validate durable events",
            )
            .expect("promotion");

        let contradictions = store
            .contradiction_ids(
                "runtime tests use cargo test and reject durable events",
                "project",
            )
            .expect("contradiction check");

        assert_eq!(contradictions.len(), 1);
    }

    #[test]
    fn expired_active_memory_does_not_block_promotion_or_contradiction_checks() {
        let store = MemoryStore::in_memory();
        let mut expired = propose_project_memory(TaskId::new(), vec![EvidenceId::new()]);
        expired.expiry = Some((Utc::now() - chrono::Duration::seconds(1)).to_rfc3339());
        store
            .promote(
                &MemoryPromotionGate::default(),
                &expired,
                "runtime tests use cargo test and validate durable events",
            )
            .expect("expired record promotion");

        assert!(
            store
                .contradiction_ids(
                    "runtime tests use cargo test and reject durable events",
                    "project",
                )
                .expect("contradiction check")
                .is_empty()
        );
        store
            .promote(
                &MemoryPromotionGate::default(),
                &propose_project_memory(TaskId::new(), vec![EvidenceId::new()]),
                "runtime tests use cargo test and validate durable events",
            )
            .expect("expired duplicate does not block replacement");
        assert_eq!(store.list().expect("records").len(), 2);
    }
}
