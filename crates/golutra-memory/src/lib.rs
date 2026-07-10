use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use chrono::{DateTime, Utc};
use golutra_core::{EvidenceId, MemoryId, TaskId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

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
        let mut records = self.load_unlocked()?;
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

    fn load_unlocked(&self) -> Result<Vec<MemoryRecord>, MemoryError> {
        let Some(path) = &self.path else {
            return self
                .in_memory_records
                .lock()
                .map(|records| records.clone())
                .map_err(|_| MemoryError::LockPoisoned);
        };
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(MemoryError::from),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(MemoryError::Io(error.to_string())),
        }
    }

    fn save_unlocked(&self, records: &[MemoryRecord]) -> Result<(), MemoryError> {
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
        fs::write(&temporary, serde_json::to_vec_pretty(records)?)
            .map_err(|error| MemoryError::Io(error.to_string()))?;
        set_owner_only_memory_file(&temporary)?;
        fs::rename(&temporary, path).map_err(|error| MemoryError::Io(error.to_string()))?;
        set_owner_only_memory_file(path)
    }
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
    let candidate_terms = terms(content);
    if candidate_terms.is_empty() {
        return Vec::new();
    }
    let candidate_content = normalize_content(content);
    records
        .iter()
        .filter(|record| record.status == MemoryStatus::Active && record.scope == scope)
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
    let lower = value.to_ascii_lowercase();
    ["sk-", "ghp_", "github_pat_"].iter().any(|prefix| {
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
}
