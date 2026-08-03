//! Validation and canonical ingestion for out-of-process evaluator results.

use std::{collections::HashMap, fs, io::Read, path::Path};

use base64::Engine;
use golutra_core::{
    ActorKind, EvidenceId, EvidenceRecord, EvidenceStrength, ExecutionOutcome,
    ExternalVerificationStatus, RedactionStatus, TaskOutcome, TaskStatus, TraceView,
    VerificationResult,
};
use golutra_eval::{
    EvaluationAttestation, EvaluationPartitionKind, EvaluationVerdict, ExternalEvaluationRecord,
    ExternalEvaluationTrust, ImportedEvaluationArtifact, external_evaluation_result_digest,
};
use golutra_protocol::{RuntimeEventSource, RuntimeEventType};
use ring::signature::{ED25519, UnparsedPublicKey};
use serde::Deserialize;

use super::*;

const MAX_TRUST_STORE_BYTES: u64 = 1024 * 1024;
const MAX_EVALUATOR_ARTIFACT_BYTES: u64 = 128 * 1024 * 1024;
const MAX_EVALUATOR_ARTIFACT_TOTAL_BYTES: u64 = 512 * 1024 * 1024;

struct ImportedEvidenceBatch {
    artifacts: Vec<(ArtifactRecord, Vec<u8>)>,
    evidence: Vec<EvidenceRecord>,
    imported: Vec<ImportedEvaluationArtifact>,
}

#[derive(Debug, Deserialize)]
struct EvaluationTrustStore {
    version: u32,
    keys: HashMap<String, EvaluationTrustKey>,
}

#[derive(Debug, Deserialize)]
struct EvaluationTrustKey {
    algorithm: String,
    public_key_base64: String,
}

impl RuntimeHost {
    pub(super) async fn handle_external_evaluation_command(
        self: &Arc<Self>,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let mut record = command
            .payload
            .get("record")
            .cloned()
            .map(serde_json::from_value::<ExternalEvaluationRecord>)
            .transpose()?
            .ok_or_else(|| {
                ClientError::InvalidSession("external evaluation record is required".to_owned())
            })?;
        let existing = {
            let evaluation_store = self.evaluation_store.clone();
            let evaluation_id = record.evaluation_id.clone();
            run_blocking(move || {
                evaluation_store.snapshot().map(|state| {
                    state
                        .external_evaluations
                        .into_iter()
                        .find(|existing| existing.evaluation_id == evaluation_id)
                })
            })
            .await??
        };
        if let Some(existing) = existing {
            if existing.result_digest != record.result_digest {
                return Err(ClientError::TaskExecution(format!(
                    "external evaluation {} already exists with different facts",
                    record.evaluation_id
                )));
            }
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: true,
                reason: Some(format!(
                    "external evaluation {} was already present",
                    record.evaluation_id
                )),
            });
        }
        self.validate_external_evaluation(session_id, &command.actor, &record)
            .await?;
        let ingestion_event_id = EventId::new();
        let artifact_base_path = command
            .payload
            .get("artifact_base_path")
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from);
        let imported = self
            .import_external_evidence(
                session_id,
                &record,
                artifact_base_path.as_deref(),
                ingestion_event_id,
            )
            .await?;
        record.imported_artifacts = imported.imported.clone();
        record.imported_evidence_refs = imported
            .evidence
            .iter()
            .map(|evidence| evidence.evidence_id)
            .collect();
        record.ingested_at = chrono::Utc::now();
        for (artifact, bytes) in &imported.artifacts {
            self.repositories.artifacts.store(artifact, bytes).await?;
        }
        for evidence in &imported.evidence {
            self.repositories.artifacts.store_evidence(evidence).await?;
        }
        let evaluation_store = self.evaluation_store.clone();
        let stored = record.clone();
        let inserted =
            run_blocking(move || evaluation_store.record_external_evaluation(stored)).await??;
        let comparison = if inserted {
            let evaluation_store = self.evaluation_store.clone();
            let evaluation_id = record.evaluation_id.clone();
            run_blocking(move || {
                evaluation_store.snapshot().map(|state| {
                    state.causal_comparisons.into_iter().find(|comparison| {
                        comparison.baseline_evaluation_ref.as_deref()
                            == Some(evaluation_id.as_str())
                            || comparison.candidate_evaluation_ref.as_deref()
                                == Some(evaluation_id.as_str())
                    })
                })
            })
            .await??
        } else {
            None
        };
        if inserted {
            let mut ingestion_event = host_event(
                self.next_sequence_no(),
                session_id,
                Some(record.source_task_id),
                RuntimeEventType::ExternalEvaluationIngested,
                RuntimeEventSource::Evaluator,
                json!({
                    "summary": format!(
                        "external evaluation {} ingested as {:?}",
                        record.evaluation_id, record.verdict
                    ),
                    "record": &record,
                    "command_id": command.command_id,
                }),
            );
            ingestion_event.id = ingestion_event_id;
            ingestion_event.payload_ref = record
                .imported_artifacts
                .first()
                .map(|artifact| artifact.artifact_ref);
            self.record_event(ingestion_event).await?;
            self.close_deferred_external_verification(
                session_id,
                record.source_task_id,
                &record,
                record.verdict,
                record.evaluation_id.clone(),
                record.imported_evidence_refs.clone(),
            )
            .await?;
            if let Some(comparison) = comparison {
                self.record_event(host_event(
                    self.next_sequence_no(),
                    session_id,
                    Some(record.source_task_id),
                    RuntimeEventType::ExternalEvaluationCompared,
                    RuntimeEventSource::Evaluator,
                    json!({
                        "summary": format!(
                            "external baseline/candidate comparison {} recorded",
                            comparison.comparison_id
                        ),
                        "record": comparison,
                        "command_id": command.command_id,
                    }),
                ))
                .await?;
            }
            self.recompute_failure_products_after_external_evaluation(
                session_id,
                record.source_task_id,
            )
            .await?;
        }
        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some(if inserted {
                format!("external evaluation {} ingested", record.evaluation_id)
            } else {
                format!(
                    "external evaluation {} was already present",
                    record.evaluation_id
                )
            }),
        })
    }

    async fn close_deferred_external_verification(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        record: &ExternalEvaluationRecord,
        verdict: EvaluationVerdict,
        evaluation_id: String,
        imported_evidence_refs: Vec<EvidenceId>,
    ) -> Result<(), ClientError> {
        let events = self
            .repositories
            .events
            .load(session_id, Some(task_id), None)
            .await?;
        let terminal = events
            .iter()
            .rev()
            .find(|event| event.event_type.is_task_terminal());
        let Some(terminal) = terminal else {
            return Ok(());
        };
        let deferred = terminal
            .payload
            .get("outcome")
            .and_then(|value| value.get("external_verification"))
            .and_then(Value::as_str)
            == Some("pending");
        if !deferred {
            return Ok(());
        }
        let status = terminal
            .payload
            .get("status")
            .cloned()
            .and_then(|value| serde_json::from_value::<TaskStatus>(value).ok())
            .unwrap_or(TaskStatus::Completed);
        let mut evidence_refs = terminal
            .payload
            .get("outcome")
            .cloned()
            .and_then(|value| serde_json::from_value::<TaskOutcome>(value).ok())
            .map(|outcome| outcome.evidence_refs)
            .unwrap_or_default();
        evidence_refs.extend(imported_evidence_refs);
        evidence_refs.sort();
        evidence_refs.dedup();
        let external_status = match verdict {
            EvaluationVerdict::Pass => ExternalVerificationStatus::Pass,
            EvaluationVerdict::Partial => ExternalVerificationStatus::Partial,
            EvaluationVerdict::Fail => ExternalVerificationStatus::Fail,
            EvaluationVerdict::Unknown => ExternalVerificationStatus::Unknown,
        };
        let pending_outcome = terminal
            .payload
            .get("outcome")
            .cloned()
            .and_then(|value| serde_json::from_value::<TaskOutcome>(value).ok())
            .unwrap_or_else(|| TaskOutcome::from_status(status, VerificationResult::Unknown));
        let was_candidate = pending_outcome.execution == ExecutionOutcome::CandidateReady;
        let outcome = pending_outcome
            .with_evidence_refs(evidence_refs)
            .with_external_verification(external_status);
        let status = if was_candidate {
            match outcome.execution {
                ExecutionOutcome::Completed => TaskStatus::Completed,
                ExecutionOutcome::Partial => TaskStatus::Partial,
                ExecutionOutcome::Failed => TaskStatus::Failed,
                ExecutionOutcome::Uncertain => TaskStatus::Uncertain,
                _ => status,
            }
        } else {
            status
        };
        self.record_event(agent_event_for_turn(
            self.next_sequence_no(),
            &HostedAgentTask {
                session_id,
                task_id,
                turn_id: terminal.turn_id.unwrap_or_default(),
                payload: Value::Null,
            },
            terminal.turn_id.unwrap_or_default(),
            RuntimeEventType::TaskCompleted,
            RuntimeEventSource::Evaluator,
            json!({
                "summary": format!("external evaluator closed deferred task verification as {verdict:?}"),
                "status": status,
                "outcome": outcome,
                "external_evaluation_id": evaluation_id,
                "post_task_governance": {"status": "pending"},
            }),
        ))
        .await?;
        let failed_assertions = record
            .assertions
            .iter()
            .filter(|assertion| !assertion.passed)
            .map(|assertion| {
                json!({
                    "name": assertion.name,
                    "message": assertion.message,
                    "evidence_refs": assertion.evidence_refs,
                })
            })
            .collect::<Vec<_>>();
        let correctable = was_candidate
            && matches!(verdict, EvaluationVerdict::Fail)
            && record
                .terminal_cause
                .as_ref()
                .is_none_or(|cause| cause.code == "assertion_failed");
        self.record_event(agent_event_for_turn(
            self.next_sequence_no(),
            &HostedAgentTask {
                session_id,
                task_id,
                turn_id: terminal.turn_id.unwrap_or_default(),
                payload: Value::Null,
            },
            terminal.turn_id.unwrap_or_default(),
            RuntimeEventType::ExternalVerificationFeedback,
            RuntimeEventSource::Evaluator,
            json!({
                "summary": if was_candidate {
                    "external evaluator feedback attached to the candidate"
                } else {
                    "external evaluator feedback attached to the terminal runtime outcome"
                },
                "evaluation_id": evaluation_id,
                "verdict": verdict,
                "failed_assertions": failed_assertions,
                "correction_available": correctable,
                "suggested_action": if correctable {
                    "resume the thread with the failed assertions and rerun the evaluator"
                } else {
                    "classify the evaluator failure before attempting a model correction"
                },
            }),
        ))
        .await
    }

    async fn import_external_evidence(
        &self,
        session_id: SessionId,
        record: &ExternalEvaluationRecord,
        artifact_base_path: Option<&Path>,
        event_id: EventId,
    ) -> Result<ImportedEvidenceBatch, ClientError> {
        let mut source_refs = record.artifact_refs.clone();
        source_refs.extend(
            record
                .assertions
                .iter()
                .flat_map(|assertion| assertion.evidence_refs.iter().cloned()),
        );
        source_refs.sort();
        source_refs.dedup();
        let canonical_base = artifact_base_path
            .map(fs::canonicalize)
            .transpose()
            .map_err(|error| {
                ClientError::TaskExecution(format!(
                    "external evaluation artifact base is invalid: {error}"
                ))
            })?;
        let mut total_bytes = 0_u64;
        let mut artifacts = Vec::new();
        let mut imported = Vec::new();
        let mut by_source = HashMap::<String, ArtifactId>::new();
        for source_ref in source_refs {
            let Some(path) = external_evidence_path(&source_ref, canonical_base.as_deref())? else {
                continue;
            };
            let metadata = fs::symlink_metadata(&path).map_err(|error| {
                ClientError::TaskExecution(format!(
                    "external evaluator evidence {} cannot be read: {error}",
                    path.display()
                ))
            })?;
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || metadata.len() > MAX_EVALUATOR_ARTIFACT_BYTES
                || total_bytes.saturating_add(metadata.len()) > MAX_EVALUATOR_ARTIFACT_TOTAL_BYTES
            {
                return Err(ClientError::TaskExecution(format!(
                    "external evaluator evidence must be a bounded regular file: {}",
                    path.display()
                )));
            }
            let remaining_total = MAX_EVALUATOR_ARTIFACT_TOTAL_BYTES.saturating_sub(total_bytes);
            let read_limit = MAX_EVALUATOR_ARTIFACT_BYTES.min(remaining_total);
            let read_path = path.clone();
            let bytes =
                run_blocking(move || read_bounded_evaluator_file(&read_path, read_limit)).await??;
            total_bytes =
                total_bytes.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
            let artifact_id = ArtifactId::new();
            let checksum = format!("sha256:{:x}", Sha256::digest(&bytes));
            let artifact = ArtifactRecord {
                artifact_id,
                session_id,
                turn_id: None,
                tool_call_id: None,
                artifact_type: "external_evaluator_evidence".to_owned(),
                uri: format!(
                    "artifact://external-evaluation/{}/{artifact_id}",
                    record.evaluation_id
                ),
                checksum: checksum.clone(),
                size_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                created_at: chrono::Utc::now(),
                producer: record.evaluator_id.clone(),
                redaction_status: RedactionStatus::Raw,
                retention_policy: "external_evaluation_owner_access".to_owned(),
                provenance_refs: vec![event_id],
            };
            by_source.insert(source_ref.clone(), artifact_id);
            imported.push(ImportedEvaluationArtifact {
                source_ref,
                artifact_ref: artifact_id,
                checksum,
                size_bytes: artifact.size_bytes,
            });
            artifacts.push((artifact, bytes));
        }
        let evidence = record
            .assertions
            .iter()
            .filter_map(|assertion| {
                let artifact_refs = assertion
                    .evidence_refs
                    .iter()
                    .filter_map(|source_ref| by_source.get(source_ref).copied())
                    .collect::<Vec<_>>();
                (!artifact_refs.is_empty()).then(|| EvidenceRecord {
                    evidence_id: EvidenceId::new(),
                    claim: format!(
                        "external assertion {}: {}",
                        assertion.name, assertion.message
                    ),
                    artifact_refs,
                    source_event_refs: vec![event_id],
                    evidence_strength: if record.trust == ExternalEvaluationTrust::Signed {
                        EvidenceStrength::Strong
                    } else {
                        EvidenceStrength::Medium
                    },
                    verifier: format!("{}:{}", record.evaluator_id, record.evaluator_version),
                    confidence: 1.0,
                    limitations: "evaluator-owned assertion imported without reinterpretation"
                        .to_owned(),
                })
            })
            .collect();
        Ok(ImportedEvidenceBatch {
            artifacts,
            evidence,
            imported,
        })
    }

    async fn recompute_failure_products_after_external_evaluation(
        &self,
        session_id: SessionId,
        task_id: TaskId,
    ) -> Result<(), ClientError> {
        let events = self
            .repositories
            .events
            .load(session_id, Some(task_id), None)
            .await?;
        let source_digest = events
            .first()
            .and_then(|event| event.payload.get("run_provenance"))
            .cloned()
            .and_then(|value| serde_json::from_value::<RunProvenance>(value).ok())
            .and_then(|provenance| provenance.build.source_digest);
        let projected_episodes = diagnosis::task_failure_episodes(task_id, &events);
        let Some(analysis) = diagnosis::diagnose_task(task_id, &events, source_digest) else {
            let evaluation_store = self.evaluation_store.clone();
            let changed =
                run_blocking(move || evaluation_store.record_failure_episodes(projected_episodes))
                    .await??;
            for episode in changed {
                self.record_event(host_event(
                    self.next_sequence_no(),
                    session_id,
                    Some(task_id),
                    RuntimeEventType::FailureEpisodeRecorded,
                    RuntimeEventSource::Evaluator,
                    json!({
                        "summary": format!(
                            "failure episode {} is {:?}",
                            episode.episode_id, episode.status
                        ),
                        "record": episode,
                    }),
                ))
                .await?;
            }
            return Ok(());
        };
        let evaluation_store = self.evaluation_store.clone();
        let analysis_for_store = analysis.clone();
        let outcome_only =
            analysis_for_store.diagnosis.layer == golutra_eval::DiagnosisLayer::Outcome;
        let update = run_blocking(move || {
            evaluation_store.record_failure_products(
                analysis_for_store.diagnosis,
                analysis_for_store.slice,
                analysis_for_store.episodes,
                (!outcome_only).then_some(analysis_for_store.candidate),
            )
        })
        .await??;
        if update.diagnosis_inserted {
            self.record_event(host_event(
                self.next_sequence_no(),
                session_id,
                Some(task_id),
                RuntimeEventType::FailureDiagnosed,
                RuntimeEventSource::Evaluator,
                json!({
                    "summary": analysis.diagnosis.summary,
                    "record": analysis.diagnosis,
                }),
            ))
            .await?;
            self.record_event(host_event(
                self.next_sequence_no(),
                session_id,
                Some(task_id),
                RuntimeEventType::DiagnosticSliceCreated,
                RuntimeEventSource::Evaluator,
                json!({
                    "summary": format!(
                        "diagnostic slice {} recomputed after external evaluation",
                        analysis.slice.slice_id
                    ),
                    "record": analysis.slice,
                }),
            ))
            .await?;
        }
        for episode in update.changed_episodes {
            self.record_event(host_event(
                self.next_sequence_no(),
                session_id,
                Some(task_id),
                RuntimeEventType::FailureEpisodeRecorded,
                RuntimeEventSource::Evaluator,
                json!({
                    "summary": format!(
                        "failure episode {} is {:?}",
                        episode.episode_id, episode.status
                    ),
                    "record": episode,
                }),
            ))
            .await?;
        }
        let candidate_id_to_process = update
            .improvement_candidate
            .as_ref()
            .map(|candidate| candidate.id.clone())
            .or_else(|| {
                update
                    .automation_candidate
                    .as_ref()
                    .map(|candidate| candidate.id.clone())
            });
        if let Some(candidate) = update.improvement_candidate {
            self.record_event(host_event(
                self.next_sequence_no(),
                session_id,
                Some(task_id),
                RuntimeEventType::ImprovementCandidateCreated,
                RuntimeEventSource::Evaluator,
                json!({
                    "summary": format!(
                        "candidate {} reprojected from external evaluator evidence",
                        candidate.id
                    ),
                    "record": candidate,
                }),
            ))
            .await?;
        }
        if let Some(candidate) = update.automation_candidate {
            self.record_event(host_event(
                self.next_sequence_no(),
                session_id,
                Some(task_id),
                RuntimeEventType::AutomationCandidateCreated,
                RuntimeEventSource::Evaluator,
                json!({
                    "summary": format!(
                        "runtime automation candidate {} synchronized from external evaluator evidence",
                        candidate.id
                    ),
                    "record": candidate,
                }),
            ))
            .await?;
        }
        if let Some(candidate_id) = candidate_id_to_process {
            Box::pin(self.automatically_process_improvement_candidate(session_id, &candidate_id))
                .await?;
        }
        Ok(())
    }

    async fn validate_external_evaluation(
        self: &Arc<Self>,
        session_id: SessionId,
        actor: &Actor,
        record: &ExternalEvaluationRecord,
    ) -> Result<(), ClientError> {
        if record.partition == EvaluationPartitionKind::Holdout && !record.holdout_protected {
            return Err(ClientError::TaskExecution(
                "holdout evaluation must declare holdout_protected=true".to_owned(),
            ));
        }
        if record.result_digest != external_evaluation_result_digest(record) {
            return Err(ClientError::TaskExecution(
                "external evaluation result_digest does not match canonical result facts"
                    .to_owned(),
            ));
        }
        let trace = TaskTraceService::new(self.clone())
            .read_complete(TaskTraceRequest {
                session_id,
                task_id: record.source_task_id,
                view: TraceView::Full,
                cursor: None,
                limit: 512,
                wait_for_evaluation: true,
            })
            .await?;
        let checkpoint_prefix = self
            .checkpoint_evaluation_tasks
            .contains(&record.source_task_id)
            && !matches!(record.trust, ExternalEvaluationTrust::UntrustedLocal)
            && checkpoint_trace_has_only_expected_incompleteness(&trace);
        if !trace.integrity.complete && !checkpoint_prefix {
            return Err(ClientError::TaskExecution(format!(
                "external evaluation base trace is incomplete: {:?}",
                trace.integrity.missing_sections
            )));
        }
        if trace.integrity.event_chain_digest != record.base_trace_digest {
            return Err(ClientError::TaskExecution(format!(
                "external evaluation base_trace_digest does not match task {}",
                record.source_task_id
            )));
        }
        if trace.runtime_identity != record.runtime_identity {
            return Err(ClientError::TaskExecution(
                "external evaluation runtime_identity does not match the source trace".to_owned(),
            ));
        }
        match record.trust {
            ExternalEvaluationTrust::UntrustedLocal => {}
            ExternalEvaluationTrust::OwnerLocal => {
                if !matches!(
                    actor.kind,
                    ActorKind::User | ActorKind::Cli | ActorKind::Tui
                ) {
                    return Err(ClientError::TaskExecution(
                        "owner-local evaluation requires an authenticated owner interaction"
                            .to_owned(),
                    ));
                }
            }
            ExternalEvaluationTrust::Signed => {
                let attestation = record.attestation.as_ref().ok_or_else(|| {
                    ClientError::TaskExecution(
                        "signed external evaluation has no attestation".to_owned(),
                    )
                })?;
                self.verify_evaluation_attestation(attestation, &record.result_digest)?;
            }
        }
        Ok(())
    }

    fn verify_evaluation_attestation(
        &self,
        attestation: &EvaluationAttestation,
        result_digest: &str,
    ) -> Result<(), ClientError> {
        let trust_path = self.evaluation_trust_store_path()?;
        verify_evaluation_attestation_at(&trust_path, attestation, result_digest)
    }

    fn evaluation_trust_store_path(&self) -> Result<std::path::PathBuf, ClientError> {
        if let Some(path) = self
            .provider_config_paths
            .as_ref()
            .and_then(|paths| paths.user_config.parent())
        {
            return Ok(path.join("evaluation-trust.json"));
        }
        self.runtime_paths
            .as_ref()
            .map(|paths| paths.home.join("evaluation-trust.json"))
            .ok_or_else(|| {
                ClientError::TaskExecution(
                    "signed evaluation trust store is unavailable in this runtime".to_owned(),
                )
            })
    }
}

pub(super) fn checkpoint_trace_has_only_expected_incompleteness(trace: &TaskTracePage) -> bool {
    const EXPECTED_MISSING_SECTIONS: [&str; 6] = [
        "context_snapshot",
        "verification_plan",
        "verification_record",
        "post_task_job",
        "post_task_job_terminal",
        "evaluation_terminal",
    ];

    trace.integrity.event_count > 0
        && trace.integrity.first_sequence.is_some()
        && trace.integrity.last_sequence.is_some()
        && trace.integrity.event_chain_digest.starts_with("sha256:")
        && !trace.has_more
        && trace.integrity.unresolved_refs.is_empty()
        && trace.integrity.retention_losses.is_empty()
        && trace.integrity.missing_causal_links.is_empty()
        && trace.integrity.orphan_events.is_empty()
        && trace.integrity.provenance_mismatches.is_empty()
        && trace.integrity.artifact_checksum_failures.is_empty()
        && trace.integrity.external_overlay_failures.is_empty()
        && trace
            .integrity
            .missing_sections
            .iter()
            .all(|section| EXPECTED_MISSING_SECTIONS.contains(&section.as_str()))
        && trace.integrity.broken_lifecycle_pairs.iter().all(|pair| {
            pair == "verification:planned_without_record"
                || ((pair.starts_with("provider_request:") || pair.starts_with("tool_call:"))
                    && pair.ends_with(":not_completed"))
        })
}

fn external_evidence_path(
    source_ref: &str,
    canonical_base: Option<&Path>,
) -> Result<Option<PathBuf>, ClientError> {
    if source_ref.contains("://") {
        return Ok(None);
    }
    let declared = Path::new(source_ref);
    if !declared.is_absolute() && canonical_base.is_none() {
        return Ok(None);
    }
    let candidate = if declared.is_absolute() {
        declared.to_path_buf()
    } else {
        canonical_base.expect("checked").join(declared)
    };
    let canonical = fs::canonicalize(&candidate).map_err(|error| {
        ClientError::TaskExecution(format!(
            "external evaluator evidence {} cannot be resolved: {error}",
            candidate.display()
        ))
    })?;
    if !declared.is_absolute() && canonical_base.is_some_and(|base| !canonical.starts_with(base)) {
        return Err(ClientError::TaskExecution(format!(
            "external evaluator evidence escapes its base directory: {}",
            candidate.display()
        )));
    }
    Ok(Some(canonical))
}

fn read_bounded_evaluator_file(path: &Path, max_bytes: u64) -> Result<Vec<u8>, ClientError> {
    let file = fs::File::open(path).map_err(|error| ClientError::Io(error.to_string()))?;
    let metadata = file
        .metadata()
        .map_err(|error| ClientError::Io(error.to_string()))?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        return Err(ClientError::TaskExecution(format!(
            "external evaluator evidence exceeds its read budget: {}",
            path.display()
        )));
    }
    let mut bytes =
        Vec::with_capacity(usize::try_from(metadata.len().min(max_bytes)).unwrap_or(usize::MAX));
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| ClientError::Io(error.to_string()))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_bytes {
        return Err(ClientError::TaskExecution(format!(
            "external evaluator evidence exceeds its read budget: {}",
            path.display()
        )));
    }
    Ok(bytes)
}

fn verify_evaluation_attestation_at(
    trust_path: &Path,
    attestation: &EvaluationAttestation,
    result_digest: &str,
) -> Result<(), ClientError> {
    if attestation.algorithm != "ed25519"
        || attestation.signed_digest != result_digest
        || attestation.key_id.trim().is_empty()
    {
        return Err(ClientError::TaskExecution(
            "external evaluation attestation metadata is invalid".to_owned(),
        ));
    }
    let trust = load_evaluation_trust_store(trust_path)?;
    let key = trust.keys.get(&attestation.key_id).ok_or_else(|| {
        ClientError::TaskExecution(format!(
            "evaluation attestation key {} is not trusted",
            attestation.key_id
        ))
    })?;
    if key.algorithm != "ed25519" {
        return Err(ClientError::TaskExecution(format!(
            "evaluation trust key {} has unsupported algorithm {}",
            attestation.key_id, key.algorithm
        )));
    }
    let public_key = base64::engine::general_purpose::STANDARD
        .decode(&key.public_key_base64)
        .map_err(|error| ClientError::TaskExecution(error.to_string()))?;
    let signature = base64::engine::general_purpose::STANDARD
        .decode(&attestation.signature)
        .map_err(|error| ClientError::TaskExecution(error.to_string()))?;
    UnparsedPublicKey::new(&ED25519, public_key)
        .verify(result_digest.as_bytes(), &signature)
        .map_err(|_| {
            ClientError::TaskExecution(
                "external evaluation attestation signature is invalid".to_owned(),
            )
        })
}

fn load_evaluation_trust_store(path: &Path) -> Result<EvaluationTrustStore, ClientError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        ClientError::TaskExecution(format!(
            "failed to read evaluation trust store {}: {error}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_TRUST_STORE_BYTES
    {
        return Err(ClientError::TaskExecution(
            "evaluation trust store must be a bounded regular file".to_owned(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o022 != 0 {
            return Err(ClientError::TaskExecution(
                "evaluation trust store cannot be group/world writable".to_owned(),
            ));
        }
    }
    let bytes = fs::read(path).map_err(|error| ClientError::Io(error.to_string()))?;
    let trust: EvaluationTrustStore = serde_json::from_slice(&bytes)?;
    if trust.version != 1 || trust.keys.is_empty() {
        return Err(ClientError::TaskExecution(
            "evaluation trust store version or key set is invalid".to_owned(),
        ));
    }
    Ok(trust)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use base64::Engine;
    use ring::signature::{Ed25519KeyPair, KeyPair};
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn signed_attestation_requires_the_trusted_key_and_exact_digest() {
        let directory = tempdir().expect("trust directory");
        let path = directory.path().join("evaluation-trust.json");
        let seed = [7_u8; 32];
        let key_pair = Ed25519KeyPair::from_seed_unchecked(&seed).expect("key pair");
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "keys": {
                    "terminal-bench": {
                        "algorithm": "ed25519",
                        "public_key_base64": base64::engine::general_purpose::STANDARD
                            .encode(key_pair.public_key().as_ref()),
                    }
                }
            }))
            .expect("trust json"),
        )
        .expect("trust store");
        let digest = "sha256:trusted-result";
        let attestation = EvaluationAttestation {
            algorithm: "ed25519".to_owned(),
            key_id: "terminal-bench".to_owned(),
            signature: base64::engine::general_purpose::STANDARD
                .encode(key_pair.sign(digest.as_bytes()).as_ref()),
            signed_digest: digest.to_owned(),
        };

        verify_evaluation_attestation_at(&path, &attestation, digest).expect("valid signature");
        assert!(verify_evaluation_attestation_at(&path, &attestation, "sha256:other").is_err());

        let mut wrong_key = attestation.clone();
        wrong_key.key_id = "unknown".to_owned();
        assert!(verify_evaluation_attestation_at(&path, &wrong_key, digest).is_err());

        let mut wrong_signature = attestation;
        wrong_signature.signature = base64::engine::general_purpose::STANDARD.encode([0_u8; 64]);
        assert!(verify_evaluation_attestation_at(&path, &wrong_signature, digest).is_err());
    }

    #[test]
    fn trust_store_rejects_invalid_version_and_unsafe_permissions() {
        let directory = tempdir().expect("trust directory");
        let path = directory.path().join("evaluation-trust.json");
        fs::write(
            &path,
            br#"{"version":2,"keys":{"key":{"algorithm":"ed25519","public_key_base64":"AA=="}}}"#,
        )
        .expect("trust store");
        assert!(load_evaluation_trust_store(&path).is_err());

        fs::write(
            &path,
            br#"{"version":1,"keys":{"key":{"algorithm":"ed25519","public_key_base64":"AA=="}}}"#,
        )
        .expect("trust store");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            fs::set_permissions(&path, fs::Permissions::from_mode(0o666))
                .expect("unsafe permissions");
            assert!(load_evaluation_trust_store(&path).is_err());
        }
    }

    #[test]
    fn evaluator_evidence_read_enforces_the_actual_byte_limit() {
        let directory = tempdir().expect("evidence directory");
        let path = directory.path().join("result.json");
        fs::write(&path, b"1234").expect("evidence file");

        assert!(read_bounded_evaluator_file(&path, 3).is_err());
        assert_eq!(
            read_bounded_evaluator_file(&path, 4).expect("bounded evidence"),
            b"1234"
        );
    }
}
