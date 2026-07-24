//! Complete task trace and artifact read service.
//!
//! Trace assembly is deliberately outside the command/query router.  It
//! joins canonical event facts with context snapshots, artifact manifests,
//! evidence and post-task jobs, and reports unresolved references instead of
//! silently returning an incomplete audit record.

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;

use base64::Engine;
use golutra_core::{
    ArtifactId, EvidenceId, PostTaskJobStatus, RunProvenance, TraceIntegrity, TraceView,
};
use golutra_eval::{ExternalEvaluationRecord, external_evaluation_result_digest};
use golutra_protocol::{ArtifactChunk, ArtifactReadRequest, TaskTracePage, TaskTraceRequest};
use golutra_store::{MAX_ARTIFACT_READ_BYTES, StoreError};
use serde_json::{Map, Value};

use super::{ClientError, RuntimeEvent, RuntimeEventType, RuntimeHost, trace_integrity};

const MAX_TRACE_PAGE_SIZE: u32 = 512;
const MAX_SUMMARY_PAGE_SIZE: u32 = 64;
pub(crate) const MAX_COMPLETE_TRACE_PAGES: usize = 4096;

/// Application service used by `RuntimeApplication` and the embedded
/// transport.  It owns no facts; all data comes from the host's canonical
/// event/artifact stores.
#[derive(Debug, Clone)]
pub struct TaskTraceService {
    host: Arc<RuntimeHost>,
}

impl TaskTraceService {
    #[must_use]
    pub(crate) fn new(host: Arc<RuntimeHost>) -> Self {
        Self { host }
    }

    pub async fn read(&self, request: TaskTraceRequest) -> Result<TaskTracePage, ClientError> {
        read_task_trace(&self.host, request).await
    }

    pub async fn read_complete(
        &self,
        request: TaskTraceRequest,
    ) -> Result<TaskTracePage, ClientError> {
        read_complete_trace(request, |request| self.read(request)).await
    }

    pub async fn read_artifact(
        &self,
        request: ArtifactReadRequest,
    ) -> Result<Option<ArtifactChunk>, ClientError> {
        read_artifact_chunk(&self.host, request).await
    }
}

pub(crate) async fn read_complete_trace<F, Fut>(
    mut request: TaskTraceRequest,
    mut read_page: F,
) -> Result<TaskTracePage, ClientError>
where
    F: FnMut(TaskTraceRequest) -> Fut,
    Fut: Future<Output = Result<TaskTracePage, ClientError>>,
{
    let mut trace = read_page(request.clone()).await?;
    for _ in 1..MAX_COMPLETE_TRACE_PAGES {
        if !trace.has_more {
            return Ok(trace);
        }
        let next_cursor = trace.next_cursor.ok_or_else(|| {
            ClientError::TaskExecution("task trace page has_more without a next cursor".to_owned())
        })?;
        if request.cursor == Some(next_cursor) {
            return Err(ClientError::TaskExecution(
                "task trace cursor did not advance".to_owned(),
            ));
        }
        request.cursor = Some(next_cursor);
        request.wait_for_evaluation = false;
        let page = read_page(request.clone()).await?;
        merge_task_trace_page(&mut trace, page)?;
    }
    if !trace.has_more {
        return Ok(trace);
    }
    Err(ClientError::TaskExecution(format!(
        "task trace exceeds {MAX_COMPLETE_TRACE_PAGES} pages"
    )))
}

/// Merges one cursor page into a complete trace while preserving integrity
/// facts and de-duplicating referenced records.
pub fn merge_task_trace_page(
    target: &mut TaskTracePage,
    page: TaskTracePage,
) -> Result<(), ClientError> {
    if target.session_id != page.session_id
        || target.task_id != page.task_id
        || target.runtime_identity != page.runtime_identity
        || target.run_provenance != page.run_provenance
        || target.view != page.view
    {
        return Err(ClientError::TaskExecution(
            "cannot merge task trace pages from different requests".to_owned(),
        ));
    }
    if target.integrity.event_chain_digest != page.integrity.event_chain_digest {
        target
            .integrity
            .unresolved_refs
            .push("integrity:event_chain_digest_mismatch".to_owned());
    }
    target.integrity.event_count = target.integrity.event_count.max(page.integrity.event_count);
    target.integrity.first_sequence = min_optional(
        target.integrity.first_sequence,
        page.integrity.first_sequence,
    );
    target.integrity.last_sequence =
        max_optional(target.integrity.last_sequence, page.integrity.last_sequence);
    target
        .integrity
        .unresolved_refs
        .extend(page.integrity.unresolved_refs);
    target
        .integrity
        .missing_sections
        .extend(page.integrity.missing_sections);
    target
        .integrity
        .retention_losses
        .extend(page.integrity.retention_losses);
    target
        .integrity
        .redacted_fields
        .extend(page.integrity.redacted_fields);
    target
        .integrity
        .missing_causal_links
        .extend(page.integrity.missing_causal_links);
    target
        .integrity
        .orphan_events
        .extend(page.integrity.orphan_events);
    target
        .integrity
        .broken_lifecycle_pairs
        .extend(page.integrity.broken_lifecycle_pairs);
    target
        .integrity
        .provenance_mismatches
        .extend(page.integrity.provenance_mismatches);
    target
        .integrity
        .artifact_checksum_failures
        .extend(page.integrity.artifact_checksum_failures);
    target
        .integrity
        .external_overlay_failures
        .extend(page.integrity.external_overlay_failures);
    target.events.extend(page.events);
    target
        .events
        .sort_by_key(|event| (event.sequence_no, event.id));
    target
        .events
        .dedup_by_key(|event| (event.sequence_no, event.id));
    target.context_snapshots.extend(page.context_snapshots);
    target
        .context_snapshots
        .sort_by_key(|snapshot| snapshot.snapshot_id);
    target
        .context_snapshots
        .dedup_by_key(|snapshot| snapshot.snapshot_id);
    target.artifacts.extend(page.artifacts);
    target
        .artifacts
        .sort_by_key(|artifact| artifact.artifact_id);
    target
        .artifacts
        .dedup_by_key(|artifact| artifact.artifact_id);
    target.evidence.extend(page.evidence);
    target.evidence.sort_by_key(|record| record.evidence_id);
    target.evidence.dedup_by_key(|record| record.evidence_id);
    if page.verification_plan.is_some() {
        target.verification_plan = page.verification_plan;
    }
    if page.verification.is_some() {
        target.verification = page.verification;
    }
    target.post_task_jobs.extend(page.post_task_jobs);
    target.post_task_jobs.sort_by_key(|job| job.job_id);
    target.post_task_jobs.dedup_by_key(|job| job.job_id);
    target.evaluation = page.evaluation;
    target.next_cursor = page.next_cursor;
    target.has_more = page.has_more;
    for values in [
        &mut target.integrity.unresolved_refs,
        &mut target.integrity.missing_sections,
        &mut target.integrity.retention_losses,
        &mut target.integrity.redacted_fields,
        &mut target.integrity.missing_causal_links,
        &mut target.integrity.orphan_events,
        &mut target.integrity.broken_lifecycle_pairs,
        &mut target.integrity.provenance_mismatches,
        &mut target.integrity.artifact_checksum_failures,
        &mut target.integrity.external_overlay_failures,
    ] {
        values.sort();
        values.dedup();
    }
    target.integrity.complete = !target.has_more
        && target.integrity.unresolved_refs.is_empty()
        && target.integrity.missing_sections.is_empty()
        && target.integrity.retention_losses.is_empty()
        && target.integrity.missing_causal_links.is_empty()
        && target.integrity.orphan_events.is_empty()
        && target.integrity.broken_lifecycle_pairs.is_empty()
        && target.integrity.provenance_mismatches.is_empty()
        && target.integrity.artifact_checksum_failures.is_empty()
        && target.integrity.external_overlay_failures.is_empty();
    Ok(())
}

fn min_optional(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (left @ Some(_), None) | (None, left) => left,
    }
}

fn max_optional(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (left @ Some(_), None) | (None, left) => left,
    }
}

pub(crate) async fn read_task_trace(
    host: &RuntimeHost,
    request: TaskTraceRequest,
) -> Result<TaskTracePage, ClientError> {
    host.ensure_task_in_session(request.session_id, request.task_id)
        .await?;
    if request.wait_for_evaluation {
        host.wait_for_deep_task_evaluation(request.task_id).await;
    }
    let repositories = &host.repositories;
    let all_task_events = repositories
        .events
        .load(request.session_id, Some(request.task_id), None)
        .await?;
    let run_provenance = all_task_events
        .first()
        .and_then(|event| event.payload.get("run_provenance"))
        .cloned()
        .and_then(|value| serde_json::from_value::<RunProvenance>(value).ok());
    let runtime_identity = run_provenance
        .as_ref()
        .map(|provenance| provenance.runtime_identity.clone())
        .unwrap_or_else(super::runtime_identity);
    let causal_integrity = trace_integrity::validate_causal_trace(
        request.task_id,
        &all_task_events,
        run_provenance.as_ref(),
    );
    let limit = request.limit.clamp(
        1,
        if request.view == TraceView::Summary {
            MAX_SUMMARY_PAGE_SIZE
        } else {
            MAX_TRACE_PAGE_SIZE
        },
    );
    let events = repositories
        .events
        .load_page(
            request.session_id,
            Some(request.task_id),
            request.cursor,
            limit.saturating_add(1),
        )
        .await?;
    let has_more = events.len() > limit as usize;
    let mut events = events;
    if has_more {
        events.pop();
    }
    let next_cursor = has_more
        .then(|| events.last().map(|event| event.sequence_no))
        .flatten();
    let mut artifact_ids = HashSet::new();
    let mut evidence_ids = HashSet::new();
    let mut verification: Option<golutra_core::VerificationRecord> = None;
    for event in &events {
        collect_artifact_refs(&event.payload, None, &mut artifact_ids);
        if let Some(artifact_id) = event.payload_ref {
            artifact_ids.insert(artifact_id);
        }
        if event.event_type == RuntimeEventType::ToolCompleted
            && let Some(envelope) = event.payload.get("envelope")
        {
            if let Some(artifact_id) = envelope
                .get("raw_artifact_ref")
                .and_then(Value::as_str)
                .and_then(|value| value.parse::<uuid::Uuid>().ok())
                .map(ArtifactId)
            {
                artifact_ids.insert(artifact_id);
            }
            if let Some(refs) = envelope.get("evidence_refs").and_then(Value::as_array) {
                evidence_ids.extend(refs.iter().filter_map(parse_evidence_id));
            }
        }
        if event.event_type == RuntimeEventType::VerificationCompleted {
            verification = event
                .payload
                .get("record")
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok());
            if let Some(record) = &verification {
                evidence_ids.extend(record.evidence_refs.iter().copied());
            }
        }
    }
    let mut artifacts = Vec::new();
    let mut unresolved_refs = Vec::new();
    let mut artifact_retention_losses = Vec::new();
    let mut artifact_checksum_failures = Vec::new();
    for artifact_id in &artifact_ids {
        match repositories.artifacts.get(*artifact_id).await? {
            Some(artifact) if artifact.session_id == request.session_id => {
                match repositories.artifacts.bytes(*artifact_id).await {
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        artifact_retention_losses.push(format!("artifact_blob:{artifact_id}"));
                    }
                    Err(StoreError::ArtifactChecksum(_)) => {
                        artifact_checksum_failures.push(artifact_id.to_string());
                    }
                    Err(StoreError::ArtifactIo(error)) => {
                        artifact_checksum_failures
                            .push(format!("{artifact_id}:artifact_io:{error}"));
                    }
                    Err(error) => return Err(error.into()),
                }
                artifacts.push(artifact);
            }
            Some(_) => unresolved_refs.push(format!("artifact:{artifact_id}:foreign_session")),
            None => unresolved_refs.push(format!("artifact:{artifact_id}")),
        }
    }
    artifacts.sort_by_key(|artifact| artifact.artifact_id);
    let artifact_id_set = artifacts
        .iter()
        .map(|artifact| artifact.artifact_id)
        .collect::<HashSet<_>>();
    let mut evidence = repositories
        .artifacts
        .evidence_by_ids(&evidence_ids)
        .await?
        .into_iter()
        .chain(
            repositories
                .artifacts
                .evidence_for_artifacts(&artifact_id_set)
                .await?
                .into_iter(),
        )
        .fold(Vec::new(), |mut records, record| {
            if !records
                .iter()
                .any(|existing: &golutra_core::EvidenceRecord| {
                    existing.evidence_id == record.evidence_id
                })
            {
                records.push(record);
            }
            records
        });
    evidence.sort_by_key(|record| record.evidence_id);
    for evidence_id in &evidence_ids {
        if !evidence
            .iter()
            .any(|record| record.evidence_id == *evidence_id)
        {
            unresolved_refs.push(format!("evidence:{evidence_id}"));
        }
    }
    if verification.is_none() {
        verification = repositories
            .projections
            .state(request.session_id, Some(request.task_id))
            .await?
            .last_verification;
    }
    let context_snapshots = repositories.artifacts.contexts(request.task_id).await?;
    let verification_plan = repositories.artifacts.verification(request.task_id).await?;
    let post_task_jobs = repositories.jobs.list_for_task(request.task_id).await?;
    let evaluation = host
        .governance
        .evaluation_projection(request.session_id, request.task_id)
        .await?;
    unresolved_refs.extend(
        evaluation
            .integrity_warnings
            .iter()
            .map(|warning| format!("evaluation:{warning}")),
    );
    unresolved_refs.sort();
    unresolved_refs.dedup();
    let event_integrity = repositories
        .events
        .integrity(request.session_id, request.task_id)
        .await?;
    let mut external_overlay_failures = Vec::new();
    for event in all_task_events
        .iter()
        .filter(|event| event.event_type == RuntimeEventType::ExternalEvaluationIngested)
    {
        let Some(value) = event.payload.get("record").cloned() else {
            external_overlay_failures.push(format!("event:{}:missing_record", event.id));
            continue;
        };
        let record = match serde_json::from_value::<ExternalEvaluationRecord>(value) {
            Ok(record) => record,
            Err(error) => {
                external_overlay_failures
                    .push(format!("event:{}:invalid_record:{error}", event.id));
                continue;
            }
        };
        let prefix = repositories
            .events
            .integrity_before(request.session_id, request.task_id, event.sequence_no)
            .await?;
        if prefix.event_chain_digest != record.base_trace_digest {
            external_overlay_failures
                .push(format!("event:{}:base_trace_digest_mismatch", event.id));
        }
        if record.runtime_identity != runtime_identity {
            external_overlay_failures.push(format!("event:{}:runtime_identity_mismatch", event.id));
        }
        if external_evaluation_result_digest(&record) != record.result_digest {
            external_overlay_failures.push(format!("event:{}:result_digest_mismatch", event.id));
        }
    }
    artifact_checksum_failures.sort();
    artifact_checksum_failures.dedup();
    external_overlay_failures.sort();
    external_overlay_failures.dedup();
    let mut missing_sections = Vec::new();
    if context_snapshots.is_empty() {
        missing_sections.push("context_snapshot".to_owned());
    }
    if verification_plan.is_none() {
        missing_sections.push("verification_plan".to_owned());
    }
    if verification.is_none() {
        missing_sections.push("verification_record".to_owned());
    }
    if post_task_jobs.is_empty() {
        missing_sections.push("post_task_job".to_owned());
    }
    if !has_more
        && post_task_jobs.iter().any(|job| {
            !matches!(
                job.status,
                PostTaskJobStatus::Succeeded
                    | PostTaskJobStatus::Failed
                    | PostTaskJobStatus::Cancelled
            )
        })
    {
        missing_sections.push("post_task_job_terminal".to_owned());
    }
    if !has_more && !evaluation.terminal {
        missing_sections.push("evaluation_terminal".to_owned());
    }
    let mut retention_losses = artifact_retention_losses;
    if request.view == TraceView::Forensic
        && context_snapshots
            .iter()
            .any(|snapshot| snapshot.restricted_request_artifact_ref.is_none())
    {
        retention_losses.push("restricted_context_capture_disabled".to_owned());
    }
    let mut redacted_fields = vec!["provider_credentials".to_owned()];
    if request.view != TraceView::Forensic {
        redacted_fields.push("restricted_context_request".to_owned());
    }
    let (events, context_snapshots, artifacts, evidence) = if request.view == TraceView::Summary {
        redacted_fields.extend([
            "event_payload_details".to_owned(),
            "context_snapshots".to_owned(),
            "artifact_manifest".to_owned(),
            "evidence_records".to_owned(),
        ]);
        (
            events.into_iter().map(summary_event).collect(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
    } else {
        (events, context_snapshots, artifacts, evidence)
    };
    redacted_fields.sort();
    redacted_fields.dedup();
    let complete = missing_sections.is_empty()
        && unresolved_refs.is_empty()
        && retention_losses.is_empty()
        && causal_integrity.missing_causal_links.is_empty()
        && causal_integrity.orphan_events.is_empty()
        && causal_integrity.broken_lifecycle_pairs.is_empty()
        && causal_integrity.provenance_mismatches.is_empty()
        && artifact_checksum_failures.is_empty()
        && external_overlay_failures.is_empty()
        && !has_more;
    Ok(TaskTracePage {
        session_id: request.session_id,
        task_id: request.task_id,
        runtime_identity,
        run_provenance,
        view: request.view,
        events,
        context_snapshots,
        artifacts,
        evidence,
        verification_plan,
        verification,
        post_task_jobs,
        evaluation,
        integrity: TraceIntegrity {
            event_count: event_integrity.event_count,
            first_sequence: event_integrity.first_sequence,
            last_sequence: event_integrity.last_sequence,
            event_chain_digest: event_integrity.event_chain_digest,
            unresolved_refs,
            missing_sections,
            retention_losses,
            redacted_fields,
            missing_causal_links: causal_integrity.missing_causal_links,
            orphan_events: causal_integrity.orphan_events,
            broken_lifecycle_pairs: causal_integrity.broken_lifecycle_pairs,
            provenance_mismatches: causal_integrity.provenance_mismatches,
            artifact_checksum_failures,
            external_overlay_failures,
            complete,
        },
        next_cursor,
        has_more,
    })
}

fn summary_event(mut event: RuntimeEvent) -> RuntimeEvent {
    const SUMMARY_FIELDS: [&str; 7] = [
        "summary", "status", "result", "decision", "action", "reason", "error",
    ];
    let mut payload = Map::new();
    if let Some(source) = event.payload.as_object() {
        for field in SUMMARY_FIELDS {
            if let Some(value) = source.get(field).filter(|value| {
                value.is_null() || value.is_boolean() || value.is_number() || value.is_string()
            }) {
                payload.insert(field.to_owned(), value.clone());
            }
        }
    }
    event.payload = Value::Object(payload);
    event.payload_ref = None;
    event
}

pub(crate) async fn read_artifact_chunk(
    host: &RuntimeHost,
    request: ArtifactReadRequest,
) -> Result<Option<ArtifactChunk>, ClientError> {
    if request.length == 0 || request.length > MAX_ARTIFACT_READ_BYTES {
        return Err(ClientError::TaskExecution(format!(
            "artifact read length must be between 1 and {MAX_ARTIFACT_READ_BYTES}"
        )));
    }
    let Some(artifact) = host.repositories.artifacts.get(request.artifact_id).await? else {
        return Ok(None);
    };
    host.ensure_owned_session_in_workspace(artifact.session_id)
        .await?;
    let Some(range) = host.repositories.artifacts.range(&request).await? else {
        return Ok(None);
    };
    let length = u64::try_from(range.bytes.len()).unwrap_or(u64::MAX);
    let end = request.offset.saturating_add(length);
    Ok(Some(ArtifactChunk {
        artifact_id: range.artifact.artifact_id,
        offset: range.offset,
        length,
        total_size: range.artifact.size_bytes,
        checksum: range.artifact.checksum,
        redaction_status: range.artifact.redaction_status,
        content_base64: base64::engine::general_purpose::STANDARD.encode(range.bytes),
        eof: end >= range.artifact.size_bytes,
    }))
}

fn parse_evidence_id(value: &Value) -> Option<EvidenceId> {
    value
        .as_str()
        .and_then(|value| value.parse::<uuid::Uuid>().ok())
        .map(EvidenceId)
}

fn collect_artifact_refs(value: &Value, key: Option<&str>, artifact_ids: &mut HashSet<ArtifactId>) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                collect_artifact_refs(value, Some(key), artifact_ids);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_artifact_refs(value, key, artifact_ids);
            }
        }
        Value::String(value)
            if key.is_some_and(|key| {
                key == "artifact" || key == "uri" || key.ends_with("_ref") || key.ends_with("_refs")
            }) =>
        {
            if let Some(artifact_id) =
                parse_artifact_ref(value, key.is_some_and(|key| key.contains("artifact")))
            {
                artifact_ids.insert(artifact_id);
            }
        }
        _ => {}
    }
}

fn parse_artifact_ref(value: &str, allow_plain_id: bool) -> Option<ArtifactId> {
    let candidate = match value.strip_prefix("artifact://") {
        Some(path) => path.split('?').next()?.rsplit('/').next()?,
        None if allow_plain_id => value,
        None => return None,
    };
    candidate.parse::<uuid::Uuid>().ok().map(ArtifactId)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn governance_artifact_refs_are_discovered_without_parsing_prompt_text() {
        let artifact_id = ArtifactId::new();
        let mut discovered = HashSet::new();
        collect_artifact_refs(
            &serde_json::json!({
                "record": {
                    "task_trace_ref": format!(
                        "artifact://regression-trace/{artifact_id}?checksum=sha256:test"
                    )
                },
                "prompt": format!("inspect artifact://regression-trace/{artifact_id}"),
            }),
            None,
            &mut discovered,
        );
        assert_eq!(discovered, HashSet::from([artifact_id]));

        let mut prompt_only = HashSet::new();
        collect_artifact_refs(
            &serde_json::json!({
                "prompt": format!("inspect artifact://regression-trace/{artifact_id}")
            }),
            None,
            &mut prompt_only,
        );
        assert!(prompt_only.is_empty());
    }
}
