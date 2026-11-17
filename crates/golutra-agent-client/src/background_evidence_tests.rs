use super::*;
use golutra_agent_core::{EvidenceId, EvidenceRecord, EvidenceStrength};
use golutra_agent_tools::ArtifactContent;

#[tokio::test]
async fn evidence_is_committed_with_terminal_bundle_and_not_inserted_twice_on_callback_retry() {
    let host = RuntimeHost::in_memory().await.unwrap();
    let (observer, request) = fixture(&host);
    let mut output = report(&request);
    let artifact_id = ArtifactId::new();
    let evidence_id = EvidenceId::new();
    let bytes = b"verified background output".to_vec();
    output.artifacts.push(ArtifactRecord {
        artifact_id,
        session_id: request.session_id,
        turn_id: request.turn_id,
        tool_call_id: Some(request.tool_call_id),
        artifact_type: "tool_result".into(),
        uri: format!("artifact://test/{artifact_id}"),
        checksum: format!("sha256:{:x}", Sha256::digest(&bytes)),
        size_bytes: bytes.len() as u64,
        created_at: chrono::Utc::now(),
        producer: "background-test".into(),
        redaction_status: RedactionStatus::NotRequired,
        retention_policy: "test".into(),
        provenance_refs: Vec::new(),
    });
    output.artifact_contents.push(ArtifactContent {
        artifact_id,
        bytes: bytes.clone(),
    });
    output.evidence.push(EvidenceRecord {
        evidence_id,
        claim: "background output exists".into(),
        artifact_refs: vec![artifact_id],
        source_event_refs: Vec::new(),
        evidence_strength: EvidenceStrength::Strong,
        verifier: "background-test".into(),
        confidence: 1.0,
        limitations: "fixture".into(),
    });
    output.envelope.raw_artifact_ref = Some(artifact_id);
    output.envelope.evidence_refs.push(evidence_id);
    observer.admit(&request).await.unwrap();
    observer.complete(&request, &output).await.unwrap();
    observer.complete(&request, &output).await.unwrap();
    let events = host
        .storage
        .repositories
        .events
        .load(request.session_id, None, None)
        .await
        .unwrap();
    let audits = events
        .iter()
        .filter(|event| event.event_type == RuntimeEventType::BackgroundTaskUpdated)
        .collect::<Vec<_>>();
    assert_eq!(audits.len(), 1);
    let evidence = host
        .storage
        .store
        .load_evidence_by_ids(&HashSet::from([evidence_id]))
        .await
        .unwrap();
    assert_eq!(evidence.len(), 1);
    assert_eq!(evidence[0].source_event_refs, vec![audits[0].id]);
    assert_eq!(evidence[0].artifact_refs, vec![artifact_id]);
    assert_eq!(
        host.storage
            .repositories
            .artifacts
            .bytes(artifact_id)
            .await
            .unwrap(),
        Some(bytes)
    );
    let results = host
        .storage
        .store
        .background()
        .pending_results(host.workspace_id, request.session_id, 128)
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].output.evidence_refs, vec![evidence_id]);
    host.close().await.unwrap();
}
