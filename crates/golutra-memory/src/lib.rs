use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex, RwLock},
    time::UNIX_EPOCH,
};

use chrono::{DateTime, Utc};
use fs2::FileExt;
use golutra_core::{EvidenceId, MemoryClaim, MemoryId, TaskId};
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const MAX_MEMORY_CONTENT_BYTES: usize = 64 * 1024;
const MAX_MEMORY_STATE_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    #[default]
    Project,
    User,
    Global,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MemoryFeedbackKind {
    Helpful,
    Irrelevant,
    Incorrect,
}

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
    #[serde(default)]
    pub claim: Option<MemoryClaim>,
    pub source_task_id: TaskId,
    pub evidence_ids: Vec<EvidenceId>,
    pub proposed_scope: MemoryScope,
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
    Proposed,
    Quarantined,
    Active,
    Deprecated,
    RolledBack,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MemoryRecord {
    pub memory_id: MemoryId,
    pub content: String,
    pub scope: MemoryScope,
    pub confidence: u8,
    pub source_task_id: TaskId,
    pub evidence_ids: Vec<EvidenceId>,
    pub contradiction_ids: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub version: u64,
    pub status: MemoryStatus,
    pub rollback_reason: Option<String>,
    #[serde(default)]
    pub supporting_task_ids: Vec<TaskId>,
    #[serde(default)]
    pub invalidation_refs: Vec<String>,
    #[serde(default)]
    pub claim: Option<MemoryClaim>,
    #[serde(default)]
    pub promotion_reviewer: Option<String>,
    #[serde(default)]
    pub helpful_count: u32,
    #[serde(default)]
    pub irrelevant_count: u32,
    #[serde(default)]
    pub incorrect_count: u32,
    #[serde(default)]
    pub access_count: u64,
    #[serde(default)]
    pub last_accessed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RetrievedMemory {
    pub record: MemoryRecord,
    pub relevance_score: u32,
    pub matched_terms: Vec<String>,
    pub reason: String,
}

#[derive(Debug)]
struct ScoredMemory {
    index: usize,
    relevance_score: u32,
    matched_terms: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MemoryPromotionDecisionKind {
    Approve,
    Reject,
    NeedsHumanReview,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MemoryPromotionDecision {
    pub decision: MemoryPromotionDecisionKind,
    pub reason: String,
    pub reviewer: Option<String>,
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
        let rejection = if candidate.evidence_ids.is_empty() {
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
                reviewer: None,
            },
            None if candidate.proposed_scope != MemoryScope::Project => MemoryPromotionDecision {
                decision: MemoryPromotionDecisionKind::NeedsHumanReview,
                reason: "user/global memory requires explicit human review".to_owned(),
                reviewer: None,
            },
            None => MemoryPromotionDecision {
                decision: MemoryPromotionDecisionKind::Approve,
                reason: "evidence-backed project memory passed the promotion gate".to_owned(),
                reviewer: Some("system".to_owned()),
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
    #[error("memory record not found: {0}")]
    NotFound(MemoryId),
    #[error("memory store limit exceeded: {0}")]
    Limit(String),
}

#[derive(Debug, Clone)]
pub struct MemoryStore {
    path: Option<PathBuf>,
    lock: Arc<Mutex<()>>,
    cached_records: Arc<RwLock<Option<MemoryCacheEntry>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MemoryFileFingerprint {
    len: u64,
    modified_nanos: u128,
}

#[derive(Debug, Clone)]
struct MemoryCacheEntry {
    fingerprint: Option<MemoryFileFingerprint>,
    records: Arc<Vec<MemoryRecord>>,
    search_index: Arc<Vec<MemorySearchIndex>>,
    /// 倒排索引把检索从“扫描全部记忆”降为“只评分命中 term 的记录”。
    /// 它与 records 共享同一快照，更新时整体替换，避免读线程看到半成品。
    term_index: Arc<HashMap<String, Vec<usize>>>,
}

#[derive(Debug, Clone)]
struct MemorySearchIndex {
    terms: HashSet<String>,
    normalized_content: String,
}

impl MemoryCacheEntry {
    fn empty(fingerprint: Option<MemoryFileFingerprint>) -> Self {
        Self {
            fingerprint,
            records: Arc::new(Vec::new()),
            search_index: Arc::new(Vec::new()),
            term_index: Arc::new(HashMap::new()),
        }
    }
}

impl MemoryStore {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Some(path.into()),
            lock: Arc::new(Mutex::new(())),
            cached_records: Arc::new(RwLock::new(None)),
        }
    }

    #[must_use]
    pub fn in_memory() -> Self {
        Self {
            path: None,
            lock: Arc::new(Mutex::new(())),
            cached_records: Arc::new(RwLock::new(None)),
        }
    }

    pub fn list(&self) -> Result<Vec<MemoryRecord>, MemoryError> {
        let _guard = self.lock.lock().map_err(|_| MemoryError::LockPoisoned)?;
        let _file_lock = self.acquire_file_lock()?;
        let mut records = self.load_unlocked()?;
        if migrate_legacy_active_memory(&mut records) || mark_expired(&mut records) {
            self.save_unlocked(&records)?;
        }
        Ok(records)
    }

    /// 成功任务只进入隔离区；隔离记录可追溯但不会污染后续上下文检索。
    pub fn quarantine(
        &self,
        candidate: &MemoryCandidate,
        content: impl Into<String>,
    ) -> Result<MemoryRecord, MemoryError> {
        let content = content.into();
        let _guard = self.lock.lock().map_err(|_| MemoryError::LockPoisoned)?;
        let _file_lock = self.acquire_file_lock()?;
        let mut records = self.load_unlocked()?;
        migrate_legacy_active_memory(&mut records);
        mark_expired(&mut records);
        let mut checked_candidate = candidate.clone();
        checked_candidate
            .contradiction_ids
            .extend(contradiction_ids_from_records(
                &records,
                &content,
                candidate.proposed_scope,
            ));
        checked_candidate.contradiction_ids.sort();
        checked_candidate.contradiction_ids.dedup();
        let decision = MemoryPromotionGate::default().evaluate(&checked_candidate, &content);
        if matches!(decision.decision, MemoryPromotionDecisionKind::Reject) {
            return Err(MemoryError::PromotionRejected(decision.reason));
        }
        let now = Utc::now();
        let candidate_id = candidate
            .claim
            .as_ref()
            .map(|claim| claim.candidate_id)
            .unwrap_or_default();
        let claim = candidate.claim.clone().or_else(|| {
            Some(MemoryClaim {
                candidate_id,
                subject: format!("scope:{:?}", candidate.proposed_scope).to_lowercase(),
                predicate: "verified_task_outcome".to_owned(),
                object: normalize_content(&content),
                scope: format!("{:?}", candidate.proposed_scope).to_lowercase(),
                source_task_refs: vec![candidate.source_task_id],
                evidence_refs: candidate.evidence_ids.clone(),
                confidence: candidate.confidence,
                valid_from: now,
                expires_at: candidate
                    .expiry
                    .as_deref()
                    .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                    .map(|value| value.with_timezone(&Utc)),
                invalidation_refs: Vec::new(),
            })
        });
        let expires_at = candidate_expiry(candidate, now)?;
        let record = MemoryRecord {
            memory_id: MemoryId::new(),
            content,
            scope: candidate.proposed_scope,
            confidence: candidate.confidence,
            source_task_id: candidate.source_task_id,
            evidence_ids: candidate.evidence_ids.clone(),
            contradiction_ids: checked_candidate.contradiction_ids,
            created_at: now,
            expires_at,
            version: next_memory_version(&records),
            status: MemoryStatus::Quarantined,
            rollback_reason: None,
            supporting_task_ids: vec![candidate.source_task_id],
            invalidation_refs: Vec::new(),
            claim,
            promotion_reviewer: None,
            helpful_count: 0,
            irrelevant_count: 0,
            incorrect_count: 0,
            access_count: 0,
            last_accessed_at: None,
        };
        records.push(record.clone());
        self.save_unlocked(&records)?;
        Ok(record)
    }

    pub fn activate_quarantined(
        &self,
        memory_id: MemoryId,
        supporting_task_ids: &[TaskId],
        reviewer: Option<&str>,
    ) -> Result<MemoryRecord, MemoryError> {
        self.activate_quarantined_with_authority(memory_id, supporting_task_ids, reviewer, false)
    }

    pub fn activate_quarantined_with_authority(
        &self,
        memory_id: MemoryId,
        supporting_task_ids: &[TaskId],
        reviewer: Option<&str>,
        human_approved: bool,
    ) -> Result<MemoryRecord, MemoryError> {
        let _guard = self.lock.lock().map_err(|_| MemoryError::LockPoisoned)?;
        let _file_lock = self.acquire_file_lock()?;
        let mut records = self.load_unlocked()?;
        migrate_legacy_active_memory(&mut records);
        mark_expired(&mut records);
        let record = records
            .iter_mut()
            .find(|record| record.memory_id == memory_id)
            .ok_or(MemoryError::NotFound(memory_id))?;
        if record.status != MemoryStatus::Quarantined {
            return Err(MemoryError::PromotionRejected(format!(
                "memory {memory_id} is not quarantined"
            )));
        }
        let mut tasks = record.supporting_task_ids.clone();
        tasks.extend(supporting_task_ids.iter().copied());
        tasks.sort();
        tasks.dedup();
        if tasks.len() < 2 && !human_approved {
            return Err(MemoryError::PromotionRejected(
                "memory activation requires two independent task evidences or human review"
                    .to_owned(),
            ));
        }
        record.supporting_task_ids = tasks;
        record.status = MemoryStatus::Active;
        record.promotion_reviewer = reviewer
            .map(ToOwned::to_owned)
            .or_else(|| Some("independent-task-evidence".to_owned()));
        let activated = record.clone();
        self.save_unlocked(&records)?;
        Ok(activated)
    }

    pub fn retrieve(
        &self,
        query: &str,
        scope: MemoryScope,
        limit: usize,
    ) -> Result<Vec<RetrievedMemory>, MemoryError> {
        let query_terms = terms(query);
        if query_terms.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        // Retrieval sits directly on the provider request hot path. It is a
        // read-only operation: avoid taking the cross-process lock and avoid
        // persisting access counters on every prompt. Governance/list paths
        // still perform the durable lifecycle updates.
        let cache = self.load_cached_for_read()?;
        let records = cache.records.as_ref();
        let now = Utc::now();
        let normalized_query = normalize_content(query);
        let candidate_indices = query_terms
            .iter()
            .filter_map(|term| cache.term_index.get(term))
            .flatten()
            .copied()
            .collect::<HashSet<_>>();
        let mut scored = candidate_indices
            .into_iter()
            .filter_map(|index| {
                let record = records.get(index)?;
                if record.status != MemoryStatus::Active
                    || !record.invalidation_refs.is_empty()
                    || is_legacy_automatic_memory(record)
                    || record.scope != scope
                    || record.expires_at.is_some_and(|expiry| expiry <= now)
                {
                    return None;
                }
                let content_index = cache.search_index.get(index)?;
                let mut matched_terms = query_terms
                    .intersection(&content_index.terms)
                    .cloned()
                    .collect::<Vec<_>>();
                matched_terms.sort();
                if matched_terms.is_empty() {
                    return None;
                }
                let phrase_bonus = u32::from(
                    !normalized_query.is_empty()
                        && content_index.normalized_content.contains(&normalized_query),
                ) * 200;
                let recency_bonus =
                    u32::from(now.signed_duration_since(record.created_at).num_days() <= 30) * 25;
                let positive = u32::try_from(matched_terms.len())
                    .unwrap_or(u32::MAX)
                    .saturating_mul(u32::from(record.confidence))
                    .saturating_add(phrase_bonus)
                    .saturating_add(recency_bonus)
                    .saturating_add(record.helpful_count.saturating_mul(50));
                let penalty = record.irrelevant_count.saturating_mul(100);
                Some(ScoredMemory {
                    index,
                    relevance_score: positive.saturating_sub(penalty),
                    matched_terms,
                })
            })
            .collect::<Vec<_>>();
        scored.sort_by(|left, right| {
            let left_record = &records[left.index];
            let right_record = &records[right.index];
            right
                .relevance_score
                .cmp(&left.relevance_score)
                .then_with(|| right_record.created_at.cmp(&left_record.created_at))
                .then_with(|| right_record.memory_id.cmp(&left_record.memory_id))
        });
        scored.truncate(limit);
        let retrieved = scored
            .into_iter()
            .map(|candidate| {
                let record = &records[candidate.index];
                RetrievedMemory {
                    relevance_score: candidate.relevance_score,
                    reason: format!(
                        "matched {} term(s), confidence {}, helpful {}, irrelevant {}",
                        candidate.matched_terms.len(),
                        record.confidence,
                        record.helpful_count,
                        record.irrelevant_count
                    ),
                    matched_terms: candidate.matched_terms,
                    record: record.clone(),
                }
            })
            .collect::<Vec<_>>();
        Ok(retrieved)
    }

    pub fn record_feedback(
        &self,
        memory_id: MemoryId,
        feedback: MemoryFeedbackKind,
        reason: impl Into<String>,
    ) -> Result<MemoryRecord, MemoryError> {
        let reason = reason.into();
        let _guard = self.lock.lock().map_err(|_| MemoryError::LockPoisoned)?;
        let _file_lock = self.acquire_file_lock()?;
        let mut records = self.load_unlocked()?;
        let record = records
            .iter_mut()
            .find(|record| record.memory_id == memory_id)
            .ok_or(MemoryError::NotFound(memory_id))?;
        match feedback {
            MemoryFeedbackKind::Helpful => {
                record.helpful_count = record.helpful_count.saturating_add(1);
            }
            MemoryFeedbackKind::Irrelevant => {
                record.irrelevant_count = record.irrelevant_count.saturating_add(1);
            }
            MemoryFeedbackKind::Incorrect => {
                record.incorrect_count = record.incorrect_count.saturating_add(1);
                record.status = MemoryStatus::RolledBack;
                let reason = if reason.trim().is_empty() {
                    "memory marked incorrect by retrieval feedback".to_owned()
                } else {
                    reason
                };
                record.invalidation_refs.push(reason.clone());
                record.rollback_reason = Some(reason);
            }
        }
        let updated = record.clone();
        self.save_unlocked(&records)?;
        Ok(updated)
    }

    pub fn contradiction_ids(
        &self,
        content: &str,
        scope: MemoryScope,
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
        let reason = reason.into();
        record.invalidation_refs.push(reason.clone());
        record.rollback_reason = Some(reason);
        let rolled_back = record.clone();
        self.save_unlocked(&records)?;
        Ok(rolled_back)
    }

    pub fn expire(&self, memory_id: MemoryId) -> Result<MemoryRecord, MemoryError> {
        let _guard = self.lock.lock().map_err(|_| MemoryError::LockPoisoned)?;
        let _file_lock = self.acquire_file_lock()?;
        let mut records = self.load_unlocked()?;
        let record = records
            .iter_mut()
            .find(|record| record.memory_id == memory_id)
            .ok_or(MemoryError::NotFound(memory_id))?;
        record.status = MemoryStatus::Expired;
        record
            .invalidation_refs
            .push("explicitly expired by runtime controller".to_owned());
        let expired = record.clone();
        self.save_unlocked(&records)?;
        Ok(expired)
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
                .cached_records
                .read()
                .map(|cache| {
                    cache
                        .as_ref()
                        .map(|entry| entry.records.as_ref().clone())
                        .unwrap_or_default()
                })
                .map_err(|_| MemoryError::LockPoisoned);
        };
        let snapshot = read_memory_file_snapshot(path)?;
        let records = match snapshot.bytes {
            Some(bytes) => serde_json::from_slice(&bytes).map_err(MemoryError::from)?,
            None => Vec::new(),
        };
        self.update_cached_records(snapshot.fingerprint, records.clone())?;
        Ok(records)
    }

    /// Return a snapshot suitable for read-heavy provider context assembly.
    /// The metadata check makes external atomic replacements visible without
    /// imposing a file lock on every retrieval. A second metadata check closes
    /// the race where a writer replaces the file while it is being parsed.
    fn load_cached_for_read(&self) -> Result<MemoryCacheEntry, MemoryError> {
        let Some(path) = &self.path else {
            return self
                .cached_records
                .read()
                .map(|cache| {
                    cache
                        .as_ref()
                        .cloned()
                        .unwrap_or_else(|| MemoryCacheEntry::empty(None))
                })
                .map_err(|_| MemoryError::LockPoisoned);
        };

        for _ in 0..2 {
            let fingerprint = memory_file_fingerprint(path)?;
            if let Some(entry) = self
                .cached_records
                .read()
                .map_err(|_| MemoryError::LockPoisoned)?
                .as_ref()
                .filter(|entry| entry.fingerprint == fingerprint)
                .cloned()
            {
                return Ok(entry);
            }

            let snapshot = read_memory_file_snapshot(path)?;
            let records = match snapshot.bytes {
                Some(bytes) => serde_json::from_slice(&bytes).map_err(MemoryError::from)?,
                None => Vec::new(),
            };
            if memory_file_fingerprint(path)? != snapshot.fingerprint {
                continue;
            }
            self.update_cached_records(snapshot.fingerprint, records)?;
            return self
                .cached_records
                .read()
                .map_err(|_| MemoryError::LockPoisoned)?
                .as_ref()
                .cloned()
                .ok_or(MemoryError::LockPoisoned);
        }

        // A highly active external writer should not make retrieval fail just
        // because metadata changed between two reads. The final snapshot is
        // still internally valid JSON and will be checked again next call.
        let snapshot = read_memory_file_snapshot(path)?;
        let records = match snapshot.bytes {
            Some(bytes) => serde_json::from_slice(&bytes).map_err(MemoryError::from)?,
            None => Vec::new(),
        };
        self.update_cached_records(snapshot.fingerprint, records)?;
        self.cached_records
            .read()
            .map_err(|_| MemoryError::LockPoisoned)?
            .as_ref()
            .cloned()
            .ok_or(MemoryError::LockPoisoned)
    }

    fn update_cached_records(
        &self,
        fingerprint: Option<MemoryFileFingerprint>,
        records: Vec<MemoryRecord>,
    ) -> Result<(), MemoryError> {
        let search_index = records
            .iter()
            .map(|record| MemorySearchIndex {
                terms: terms(&record.content),
                normalized_content: normalize_content(&record.content),
            })
            .collect::<Vec<_>>();
        let mut term_index = HashMap::<String, Vec<usize>>::new();
        for (index, content_index) in search_index.iter().enumerate() {
            for term in &content_index.terms {
                term_index.entry(term.clone()).or_default().push(index);
            }
        }
        *self
            .cached_records
            .write()
            .map_err(|_| MemoryError::LockPoisoned)? = Some(MemoryCacheEntry {
            fingerprint,
            search_index: Arc::new(search_index),
            records: Arc::new(records),
            term_index: Arc::new(term_index),
        });
        Ok(())
    }

    fn save_unlocked(&self, records: &[MemoryRecord]) -> Result<(), MemoryError> {
        let encoded = serde_json::to_vec_pretty(records)?;
        if u64::try_from(encoded.len()).unwrap_or(u64::MAX) > MAX_MEMORY_STATE_BYTES {
            return Err(MemoryError::Limit(format!(
                "serialized state exceeds {MAX_MEMORY_STATE_BYTES} bytes"
            )));
        }
        let Some(path) = &self.path else {
            self.update_cached_records(None, records.to_vec())?;
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
        self.update_cached_records(memory_file_fingerprint(path)?, records.to_vec())?;
        Ok(())
    }
}

#[derive(Debug)]
struct MemoryFileSnapshot {
    fingerprint: Option<MemoryFileFingerprint>,
    bytes: Option<Vec<u8>>,
}

fn memory_file_fingerprint(path: &Path) -> Result<Option<MemoryFileFingerprint>, MemoryError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(MemoryError::Io(error.to_string())),
    };
    let modified_nanos = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |value| value.as_nanos());
    Ok(Some(MemoryFileFingerprint {
        len: metadata.len(),
        modified_nanos,
    }))
}

fn read_memory_file_snapshot(path: &Path) -> Result<MemoryFileSnapshot, MemoryError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(MemoryFileSnapshot {
                fingerprint: None,
                bytes: None,
            });
        }
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
    let modified_nanos = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |value| value.as_nanos());
    Ok(MemoryFileSnapshot {
        fingerprint: Some(MemoryFileFingerprint {
            len: metadata.len(),
            modified_nanos,
        }),
        bytes: Some(bytes),
    })
}

#[must_use]
pub fn propose_project_memory(
    source_task_id: TaskId,
    evidence_ids: Vec<EvidenceId>,
) -> MemoryCandidate {
    let candidate_id = golutra_core::MemoryCandidateId::new();
    let valid_from = Utc::now();
    MemoryCandidate {
        claim: Some(MemoryClaim {
            candidate_id,
            subject: "project".to_owned(),
            predicate: "verified_task_outcome".to_owned(),
            object: format!("task:{source_task_id}"),
            scope: "project".to_owned(),
            source_task_refs: vec![source_task_id],
            evidence_refs: evidence_ids.clone(),
            confidence: 80,
            valid_from,
            expires_at: Some(valid_from + chrono::Duration::days(30)),
            invalidation_refs: Vec::new(),
        }),
        source_task_id,
        evidence_ids,
        proposed_scope: MemoryScope::Project,
        confidence: 80,
        contradiction_ids: Vec::new(),
        expiry: Some((valid_from + chrono::Duration::days(30)).to_rfc3339()),
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

fn candidate_expiry(
    candidate: &MemoryCandidate,
    now: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>, MemoryError> {
    let parsed = candidate
        .expiry
        .as_deref()
        .map(DateTime::parse_from_rfc3339)
        .transpose()
        .map_err(|error| MemoryError::Io(error.to_string()))?
        .map(|value| value.with_timezone(&Utc));
    Ok(parsed.or_else(|| {
        (candidate.proposed_scope == MemoryScope::Project).then(|| now + chrono::Duration::days(30))
    }))
}

fn next_memory_version(records: &[MemoryRecord]) -> u64 {
    records
        .iter()
        .map(|record| record.version)
        .max()
        .unwrap_or_default()
        .saturating_add(1)
}

fn migrate_legacy_active_memory(records: &mut [MemoryRecord]) -> bool {
    let mut changed = false;
    for record in records {
        if is_legacy_automatic_memory(record) {
            record.status = MemoryStatus::Quarantined;
            record.invalidation_refs.push(
                "legacy single-task project memory requires independent evidence or review"
                    .to_owned(),
            );
            changed = true;
        }
    }
    changed
}

fn is_legacy_automatic_memory(record: &MemoryRecord) -> bool {
    record.status == MemoryStatus::Active
        && record.scope == MemoryScope::Project
        && record.claim.is_none()
        && record.supporting_task_ids.is_empty()
        && record
            .promotion_reviewer
            .as_deref()
            .is_none_or(|reviewer| reviewer == "system")
}

fn mark_expired(records: &mut [MemoryRecord]) -> bool {
    let now = Utc::now();
    let mut changed = false;
    for record in records {
        if record.status == MemoryStatus::Active
            && record
                .expires_at
                .is_some_and(|expires_at| expires_at <= now)
        {
            record.status = MemoryStatus::Expired;
            record
                .invalidation_refs
                .push("memory expiry reached".to_owned());
            changed = true;
        }
    }
    changed
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
    scope: MemoryScope,
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
    fn activates_retrieves_and_rolls_back_project_memory() {
        let directory = tempdir().expect("directory");
        let store = MemoryStore::new(directory.path().join("memory.json"));
        let candidate = propose_project_memory(TaskId::new(), vec![EvidenceId::new()]);

        let promoted =
            activate_project_memory(&store, &candidate, "cargo test validates the runtime store");
        let retrieved = store
            .retrieve("validate runtime with cargo test", MemoryScope::Project, 3)
            .expect("retrieval");
        let rolled_back = store
            .rollback(promoted.memory_id, "superseded")
            .expect("rollback");

        assert_eq!(retrieved.len(), 1);
        assert_eq!(retrieved[0].record.memory_id, promoted.memory_id);
        assert_eq!(rolled_back.status, MemoryStatus::RolledBack);
        assert!(
            store
                .retrieve("cargo test runtime", MemoryScope::Project, 3)
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
    fn user_memory_requires_human_review_and_feedback_affects_lifecycle() {
        let store = MemoryStore::in_memory();
        let mut candidate = propose_project_memory(TaskId::new(), vec![EvidenceId::new()]);
        candidate.proposed_scope = MemoryScope::User;
        let gate = MemoryPromotionGate::default();

        assert_eq!(
            gate.evaluate(&candidate, "use cargo test for runtime changes")
                .decision,
            MemoryPromotionDecisionKind::NeedsHumanReview
        );
        let quarantined = store
            .quarantine(&candidate, "use cargo test for runtime changes")
            .expect("quarantine");
        assert!(
            store
                .retrieve("cargo test runtime", MemoryScope::User, 3)
                .expect("quarantine retrieval")
                .is_empty()
        );
        let record = store
            .activate_quarantined_with_authority(
                quarantined.memory_id,
                &[],
                Some("maintainer-1"),
                true,
            )
            .expect("human activation");
        let helpful = store
            .record_feedback(record.memory_id, MemoryFeedbackKind::Helpful, "reused")
            .expect("helpful feedback");
        let incorrect = store
            .record_feedback(record.memory_id, MemoryFeedbackKind::Incorrect, "outdated")
            .expect("incorrect feedback");

        assert_eq!(record.promotion_reviewer.as_deref(), Some("maintainer-1"));
        assert_eq!(helpful.helpful_count, 1);
        assert_eq!(incorrect.status, MemoryStatus::RolledBack);
        assert!(
            store
                .retrieve("cargo test runtime", MemoryScope::User, 3)
                .expect("retrieve")
                .is_empty()
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
        activate_project_memory(
            &cloned,
            &propose_project_memory(TaskId::new(), vec![EvidenceId::new()]),
            "workspace tests use cargo test",
        );

        assert_eq!(store.list().expect("list").len(), 1);
    }

    #[test]
    fn file_backed_store_preserves_updates_from_independent_instances() {
        let directory = tempdir().expect("directory");
        let path = directory.path().join("memory.json");
        let first = MemoryStore::new(&path);
        let second = MemoryStore::new(&path);

        activate_project_memory(
            &first,
            &propose_project_memory(TaskId::new(), vec![EvidenceId::new()]),
            "first process validates cargo tests",
        );
        activate_project_memory(
            &second,
            &propose_project_memory(TaskId::new(), vec![EvidenceId::new()]),
            "second process validates runtime events",
        );

        assert_eq!(first.list().expect("shared records").len(), 2);
        assert!(path.with_extension("lock").exists());
    }

    #[test]
    fn retrieval_is_read_only_and_does_not_rewrite_access_metadata() {
        let directory = tempdir().expect("directory");
        let path = directory.path().join("memory.json");
        let store = MemoryStore::new(&path);
        activate_project_memory(
            &store,
            &propose_project_memory(TaskId::new(), vec![EvidenceId::new()]),
            "cached runtime retrieval stays cheap",
        );
        let before = fs::read(&path).expect("memory before retrieval");

        let first = store
            .retrieve("cached runtime", MemoryScope::Project, 3)
            .expect("first retrieval");
        let second = store
            .retrieve("cached runtime", MemoryScope::Project, 3)
            .expect("second retrieval");
        let after = fs::read(&path).expect("memory after retrieval");

        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_eq!(before, after, "provider retrieval must not rewrite state");
        assert_eq!(second[0].record.access_count, 0);
    }

    #[test]
    fn read_snapshot_invalidates_after_an_independent_file_update() {
        let directory = tempdir().expect("directory");
        let path = directory.path().join("memory.json");
        let first = MemoryStore::new(&path);
        let second = MemoryStore::new(&path);

        assert!(
            first
                .retrieve("new runtime fact", MemoryScope::Project, 3)
                .expect("empty retrieval")
                .is_empty()
        );
        activate_project_memory(
            &second,
            &propose_project_memory(TaskId::new(), vec![EvidenceId::new()]),
            "new runtime fact is persisted by another process",
        );

        let retrieved = first
            .retrieve("new runtime fact", MemoryScope::Project, 3)
            .expect("retrieval after external update");
        assert_eq!(retrieved.len(), 1);
    }

    #[test]
    fn contradiction_check_flags_near_duplicate_facts() {
        let store = MemoryStore::in_memory();
        activate_project_memory(
            &store,
            &propose_project_memory(TaskId::new(), vec![EvidenceId::new()]),
            "runtime tests use cargo test and validate durable events",
        );

        let contradictions = store
            .contradiction_ids(
                "runtime tests use cargo test and reject durable events",
                MemoryScope::Project,
            )
            .expect("contradiction check");

        assert_eq!(contradictions.len(), 1);
    }

    #[test]
    fn expired_active_memory_does_not_block_promotion_or_contradiction_checks() {
        let store = MemoryStore::in_memory();
        let mut expired = propose_project_memory(TaskId::new(), vec![EvidenceId::new()]);
        expired.expiry = Some((Utc::now() - chrono::Duration::seconds(1)).to_rfc3339());
        activate_project_memory(
            &store,
            &expired,
            "runtime tests use cargo test and validate durable events",
        );

        assert!(
            store
                .contradiction_ids(
                    "runtime tests use cargo test and reject durable events",
                    MemoryScope::Project,
                )
                .expect("contradiction check")
                .is_empty()
        );
        activate_project_memory(
            &store,
            &propose_project_memory(TaskId::new(), vec![EvidenceId::new()]),
            "runtime tests use cargo test and validate durable events",
        );
        assert_eq!(store.list().expect("records").len(), 2);
    }

    #[test]
    fn quarantine_requires_independent_task_evidence_before_activation() {
        let store = MemoryStore::in_memory();
        let candidate = propose_project_memory(TaskId::new(), vec![EvidenceId::new()]);
        let record = store
            .quarantine(&candidate, "runtime changes require cargo test")
            .expect("quarantine");

        assert!(
            store
                .retrieve("runtime cargo test", MemoryScope::Project, 3)
                .expect("retrieve quarantine")
                .is_empty()
        );
        assert!(matches!(
            store.activate_quarantined(record.memory_id, &[], None),
            Err(MemoryError::PromotionRejected(_))
        ));
        let active = store
            .activate_quarantined(record.memory_id, &[TaskId::new()], None)
            .expect("independent evidence activation");

        assert_eq!(active.status, MemoryStatus::Active);
        assert_eq!(active.supporting_task_ids.len(), 2);
        assert_eq!(
            store
                .retrieve("runtime cargo test", MemoryScope::Project, 3)
                .expect("retrieve active")
                .len(),
            1
        );
    }

    #[test]
    fn legacy_single_task_active_memory_is_migrated_to_quarantine() {
        let directory = tempdir().expect("directory");
        let path = directory.path().join("memory.json");
        let task_id = TaskId::new();
        let legacy = MemoryRecord {
            memory_id: MemoryId::new(),
            content: "legacy runtime fact".to_owned(),
            scope: MemoryScope::Project,
            confidence: 80,
            source_task_id: task_id,
            evidence_ids: vec![EvidenceId::new()],
            contradiction_ids: Vec::new(),
            created_at: Utc::now(),
            expires_at: None,
            version: 1,
            status: MemoryStatus::Active,
            rollback_reason: None,
            supporting_task_ids: Vec::new(),
            invalidation_refs: Vec::new(),
            claim: None,
            promotion_reviewer: Some("system".to_owned()),
            helpful_count: 0,
            irrelevant_count: 0,
            incorrect_count: 0,
            access_count: 0,
            last_accessed_at: None,
        };
        fs::write(
            &path,
            serde_json::to_vec_pretty(&vec![legacy]).expect("legacy JSON"),
        )
        .expect("legacy state");

        let migrated = MemoryStore::new(&path).list().expect("migrated state");

        assert_eq!(migrated[0].status, MemoryStatus::Quarantined);
        assert!(
            migrated[0]
                .invalidation_refs
                .iter()
                .any(|reason| reason.contains("independent evidence"))
        );
        let persisted: Vec<MemoryRecord> =
            serde_json::from_slice(&fs::read(&path).expect("persisted state"))
                .expect("persisted JSON");
        assert_eq!(persisted[0].status, MemoryStatus::Quarantined);
    }

    fn activate_project_memory(
        store: &MemoryStore,
        candidate: &MemoryCandidate,
        content: &str,
    ) -> MemoryRecord {
        let quarantined = store
            .quarantine(candidate, content)
            .expect("memory quarantine");
        store
            .activate_quarantined(quarantined.memory_id, &[TaskId::new()], None)
            .expect("memory activation")
    }
}
