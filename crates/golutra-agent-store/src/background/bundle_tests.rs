use super::*;
use golutra_agent_core::{
    ArtifactRecord, EvidenceId, EvidenceRecord, EvidenceStrength, RedactionStatus,
};
use golutra_agent_protocol::RuntimeEventSource;
use sha2::{Digest, Sha256};
use std::collections::HashSet;

struct CompletionBundle {
    result: BackgroundResult,
    event: RuntimeEvent,
    artifact: ArtifactRecord,
    bytes: Vec<u8>,
    evidence: EvidenceRecord,
}

fn completion_bundle(task: &BackgroundTaskRecord) -> CompletionBundle {
    let mut result = result(task);
    let event_id = EventId(uuid::Uuid::new_v5(
        &result.result_id.0,
        b"background-result-event",
    ));
    let artifact_id = ArtifactId::new();
    let evidence_id = EvidenceId::new();
    let bytes = b"background result captured".to_vec();
    result.artifact_refs.push(artifact_id.to_string());
    result.output.raw_artifact_ref = Some(artifact_id);
    result.output.evidence_refs.push(evidence_id);
    let artifact = ArtifactRecord {
        artifact_id,
        session_id: task.scope.session_id,
        turn_id: Some(task.scope.turn_id),
        tool_call_id: Some(task.tool_call_id),
        artifact_type: "tool_result".into(),
        uri: format!("artifact://background/{artifact_id}"),
        checksum: format!("sha256:{:x}", Sha256::digest(&bytes)),
        size_bytes: bytes.len() as u64,
        created_at: result.created_at,
        producer: "test".into(),
        redaction_status: RedactionStatus::NotRequired,
        retention_policy: "test".into(),
        provenance_refs: vec![event_id],
    };
    let evidence = EvidenceRecord {
        evidence_id,
        claim: "background tool completed".into(),
        artifact_refs: vec![artifact_id],
        source_event_refs: vec![event_id],
        evidence_strength: EvidenceStrength::Strong,
        verifier: "test".into(),
        confidence: 1.0,
        limitations: "fixture".into(),
    };
    let event = RuntimeEvent {
        schema_version: golutra_agent_core::RUNTIME_EVENT_SCHEMA_VERSION,
        causal_context: Default::default(),
        causal_links: Vec::new(),
        id: event_id,
        sequence_no: 0,
        session_id: task.scope.session_id,
        turn_id: Some(task.scope.turn_id),
        task_id: Some(task.scope.task_id),
        parent_event_id: None,
        event_type: RuntimeEventType::BackgroundTaskUpdated,
        timestamp: result.created_at,
        source: RuntimeEventSource::Tool,
        payload: serde_json::json!({
            "summary":result.summary,
            "envelope":result.output,
            "background_task_id":result.background_task_id,
            "batch_id":result.batch_id,
            "result_id":result.result_id,
            "status":result.status,
        }),
        payload_ref: Some(artifact_id),
        durable: true,
    };
    CompletionBundle {
        result,
        event,
        artifact,
        bytes,
        evidence,
    }
}

#[tokio::test]
async fn completion_audit_and_inbox_commit_atomically_with_artifacts_and_evidence() {
    let store = RuntimeStore::in_memory().await.unwrap();
    let repo = store.background();
    let batch = batch();
    let task = task(&batch);
    repo.create_batch(&batch).await.unwrap();
    repo.register_task(&task).await.unwrap();
    let bundle = completion_bundle(&task);
    let saved = store
        .repositories()
        .events
        .append_background_completed_bundle(
            bundle.event,
            &[(bundle.artifact.clone(), bundle.bytes.clone())],
            std::slice::from_ref(&bundle.evidence),
            &bundle.result,
        )
        .await
        .unwrap();

    assert_eq!(store.event_by_id(saved.id).await.unwrap(), Some(saved));
    assert_eq!(
        repo.result_for_task(task.background_task_id).await.unwrap(),
        Some(bundle.result.clone())
    );
    assert_eq!(
        repo.pending_results(batch.scope.workspace_id, batch.scope.session_id, 10)
            .await
            .unwrap(),
        vec![bundle.result]
    );
    assert_eq!(
        store
            .load_artifact(bundle.artifact.artifact_id)
            .await
            .unwrap(),
        Some(bundle.artifact)
    );
    assert_eq!(
        store
            .load_evidence_by_ids(&HashSet::from([bundle.evidence.evidence_id]))
            .await
            .unwrap(),
        vec![bundle.evidence]
    );
    assert_eq!(
        repo.recover_abandoned(batch.scope.workspace_id, &[batch.scope.task_id])
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        repo.tasks(batch.batch_id).await.unwrap()[0].status,
        BackgroundTaskStatus::Succeeded
    );
}

#[tokio::test]
async fn completion_event_conflict_rolls_back_inbox_task_state_and_facts() {
    let store = RuntimeStore::in_memory().await.unwrap();
    let repo = store.background();
    let batch = batch();
    let task = task(&batch);
    repo.create_batch(&batch).await.unwrap();
    repo.register_task(&task).await.unwrap();
    let bundle = completion_bundle(&task);
    let mut conflict = bundle.event.clone();
    conflict.event_type = RuntimeEventType::ToolStarted;
    conflict.payload = serde_json::json!({"summary":"conflicting identity"});
    conflict.payload_ref = None;
    let conflict = store
        .append_event_assigning_sequence(conflict)
        .await
        .unwrap();
    assert!(
        store
            .append_background_completed_bundle(
                bundle.event,
                &[(bundle.artifact.clone(), bundle.bytes)],
                std::slice::from_ref(&bundle.evidence),
                &bundle.result,
            )
            .await
            .is_err()
    );

    assert_eq!(
        store.event_by_id(conflict.id).await.unwrap(),
        Some(conflict)
    );
    assert!(
        repo.result_for_task(task.background_task_id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        repo.tasks(batch.batch_id).await.unwrap()[0].status,
        BackgroundTaskStatus::Queued
    );
    assert!(
        store
            .load_artifact(bundle.artifact.artifact_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !store
            .artifact_blob_path(bundle.artifact.artifact_id)
            .exists()
    );
    assert!(
        store
            .load_evidence_by_ids(&HashSet::from([bundle.evidence.evidence_id]))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn mismatched_completion_identity_and_persisted_scope_cannot_publish_a_result() {
    let store = RuntimeStore::in_memory().await.unwrap();
    let repo = store.background();
    let batch = batch();
    let task = task(&batch);
    repo.create_batch(&batch).await.unwrap();
    repo.register_task(&task).await.unwrap();
    let bundle = completion_bundle(&task);
    let mut wrong_event = bundle.event.clone();
    wrong_event.id = EventId::new();
    assert!(
        store
            .append_background_completed_bundle(
                wrong_event,
                &[(bundle.artifact.clone(), bundle.bytes.clone())],
                std::slice::from_ref(&bundle.evidence),
                &bundle.result,
            )
            .await
            .is_err()
    );

    // Event and result agree with each other, but not the durably admitted task.
    let mut wrong_result = bundle.result.clone();
    wrong_result.scope.workspace_id = WorkspaceId::new();
    assert!(
        store
            .append_background_completed_bundle(
                bundle.event.clone(),
                &[(bundle.artifact.clone(), bundle.bytes)],
                std::slice::from_ref(&bundle.evidence),
                &wrong_result,
            )
            .await
            .is_err()
    );
    assert!(store.event_by_id(bundle.event.id).await.unwrap().is_none());
    assert!(
        repo.result_for_task(task.background_task_id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        repo.tasks(batch.batch_id).await.unwrap()[0].status,
        BackgroundTaskStatus::Queued
    );
    assert!(
        store
            .load_artifact(bundle.artifact.artifact_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !store
            .artifact_blob_path(bundle.artifact.artifact_id)
            .exists()
    );
    assert!(
        store
            .load_evidence_by_ids(&HashSet::from([bundle.evidence.evidence_id]))
            .await
            .unwrap()
            .is_empty()
    );
}
