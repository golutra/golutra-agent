use super::*;
use golutra_agent_core::BackgroundScope;

#[path = "bundle_tests.rs"]
mod bundle_tests;

fn batch() -> BackgroundBatch {
    let now = Utc::now();
    BackgroundBatch {
        batch_id: BackgroundBatchId::new(),
        scope: BackgroundScope {
            workspace_id: WorkspaceId::new(),
            session_id: SessionId::new(),
            task_id: TaskId::new(),
            turn_id: TurnId::new(),
        },
        generation: 3,
        created_at: now,
        deadline_at: now + Duration::minutes(3),
        sealed: false,
    }
}

fn task(batch: &BackgroundBatch) -> BackgroundTaskRecord {
    BackgroundTaskRecord {
        background_task_id: BackgroundTaskId::new(),
        batch_id: batch.batch_id,
        scope: batch.scope,
        tool_call_id: ToolCallId::new(),
        tool_name: "browser_run".to_owned(),
        backend: "ego".to_owned(),
        status: BackgroundTaskStatus::Queued,
        created_at: batch.created_at,
        deadline_at: batch.deadline_at,
    }
}

fn result(task: &BackgroundTaskRecord) -> BackgroundResult {
    BackgroundResult {
        result_id: BackgroundResultId::new(),
        background_task_id: task.background_task_id,
        batch_id: task.batch_id,
        scope: task.scope,
        tool_call_id: task.tool_call_id,
        tool_name: task.tool_name.clone(),
        status: BackgroundTaskStatus::Succeeded,
        summary: "page loaded".to_owned(),
        artifact_refs: vec![],
        created_at: Utc::now(),
        output: ToolResultEnvelope {
            tool_call_id: task.tool_call_id,
            tool_name: task.tool_name.clone(),
            status: ToolResultStatus::Ok,
            summary: "page loaded".to_owned(),
            structured_facts: serde_json::json!({"url":"https://example.com"}),
            model_visible_excerpt: None,
            raw_artifact_ref: None,
            evidence_refs: vec![],
            risk: "none".to_owned(),
            verification_hint: None,
        },
    }
}

async fn claim(
    repo: &BackgroundRepository,
    batch: &BackgroundBatch,
    owner: &str,
) -> Option<BackgroundWakeClaim> {
    repo.claim_batch_wake(
        batch.batch_id,
        batch.generation,
        owner,
        Utc::now(),
        Duration::seconds(30),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn sealed_batch_waits_for_every_operation_and_only_one_wake_claim_wins() {
    let store = RuntimeStore::in_memory().await.unwrap();
    let repo = store.background();
    let batch = batch();
    repo.create_batch(&batch).await.unwrap();
    let first = task(&batch);
    let second = task(&batch);
    repo.register_task(&first).await.unwrap();
    repo.register_task(&second).await.unwrap();
    repo.finish_task(&result(&first)).await.unwrap();
    assert!(claim(&repo, &batch, "early").await.is_none());
    repo.seal_batch(batch.batch_id).await.unwrap();
    assert!(claim(&repo, &batch, "partial").await.is_none());
    repo.finish_task(&result(&second)).await.unwrap();
    let (a, b) = tokio::join!(claim(&repo, &batch, "a"), claim(&repo, &batch, "b"));
    assert_eq!(usize::from(a.is_some()) + usize::from(b.is_some()), 1);
    let lease = a.or(b).unwrap();
    assert!(repo.commit_wake(&lease).await.unwrap());
    assert!(!repo.commit_wake(&lease).await.unwrap());
    assert!(claim(&repo, &batch, "again").await.is_none());
}

#[tokio::test]
async fn completion_before_seal_is_retained_and_duplicate_completion_cannot_replace_it() {
    let store = RuntimeStore::in_memory().await.unwrap();
    let repo = store.background();
    let batch = batch();
    repo.create_batch(&batch).await.unwrap();
    let task = task(&batch);
    repo.register_task(&task).await.unwrap();
    let original = result(&task);
    assert!(repo.finish_task(&original).await.unwrap());
    let mut duplicate = result(&task);
    duplicate.status = BackgroundTaskStatus::Failed;
    duplicate.summary = "late cancellation cannot rewrite completion".to_owned();
    assert!(!repo.finish_task(&duplicate).await.unwrap());
    assert_eq!(
        repo.result_for_task(task.background_task_id).await.unwrap(),
        Some(original.clone())
    );
    assert!(claim(&repo, &batch, "before").await.is_none());
    repo.seal_batch(batch.batch_id).await.unwrap();
    assert!(claim(&repo, &batch, "after").await.is_some());
    assert_eq!(
        repo.pending_results(batch.scope.workspace_id, batch.scope.session_id, 100)
            .await
            .unwrap(),
        vec![original]
    );
}

#[tokio::test]
async fn user_consumption_and_stop_both_prevent_reserved_wake() {
    for stop in [false, true] {
        let store = RuntimeStore::in_memory().await.unwrap();
        let repo = store.background();
        let batch = batch();
        repo.create_batch(&batch).await.unwrap();
        let task = task(&batch);
        repo.register_task(&task).await.unwrap();
        let output = result(&task);
        repo.finish_task(&output).await.unwrap();
        repo.seal_batch(batch.batch_id).await.unwrap();
        let lease = claim(&repo, &batch, "wake").await.unwrap();
        if stop {
            repo.suppress_session(batch.scope.workspace_id, batch.scope.session_id)
                .await
                .unwrap();
        } else {
            assert_eq!(
                repo.acknowledge_results(
                    batch.scope.workspace_id,
                    batch.scope.session_id,
                    &[output.result_id],
                    TurnId::new()
                )
                .await
                .unwrap(),
                1
            );
        }
        assert!(!repo.commit_wake(&lease).await.unwrap());
        repo.release_wake(&lease).await.unwrap();
        assert!(claim(&repo, &batch, "later").await.is_none());
        // Acknowledged results remain inspectable for retries and audit.
        assert_eq!(
            repo.result_for_task(task.background_task_id).await.unwrap(),
            Some(output.clone())
        );
        assert!(
            repo.result_by_id(WorkspaceId::new(), batch.scope.session_id, output.result_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            repo.result_by_id(batch.scope.workspace_id, SessionId::new(), output.result_id)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            repo.result_by_id(
                batch.scope.workspace_id,
                batch.scope.session_id,
                output.result_id
            )
            .await
            .unwrap(),
            Some(output)
        );
    }
}

#[tokio::test]
async fn batch_suppression_is_scoped_and_preserves_newer_batches_and_pending_results() {
    let store = RuntimeStore::in_memory().await.unwrap();
    let repo = store.background();
    let older = batch();
    let mut newer = older.clone();
    newer.batch_id = BackgroundBatchId::new();
    newer.scope.task_id = TaskId::new();
    newer.scope.turn_id = TurnId::new();
    newer.generation += 1;
    repo.create_batch(&older).await.unwrap();
    repo.create_batch(&newer).await.unwrap();
    let old_task = task(&older);
    let new_task = task(&newer);
    repo.register_task(&old_task).await.unwrap();
    repo.register_task(&new_task).await.unwrap();
    let old_result = result(&old_task);
    repo.finish_task(&old_result).await.unwrap();
    repo.finish_task(&result(&new_task)).await.unwrap();
    repo.seal_batch(older.batch_id).await.unwrap();
    let old_claim = claim(&repo, &older, "old-owner").await.unwrap();

    assert!(
        !repo
            .suppress_batch(WorkspaceId::new(), older.scope.session_id, older.batch_id)
            .await
            .unwrap()
    );
    assert!(
        !repo
            .suppress_batch(older.scope.workspace_id, SessionId::new(), older.batch_id)
            .await
            .unwrap()
    );
    assert!(
        repo.suppress_batch(
            older.scope.workspace_id,
            older.scope.session_id,
            older.batch_id,
        )
        .await
        .unwrap()
    );
    assert!(
        !repo
            .suppress_batch(
                older.scope.workspace_id,
                older.scope.session_id,
                older.batch_id,
            )
            .await
            .unwrap()
    );
    assert!(!repo.commit_wake(&old_claim).await.unwrap());
    assert!(!repo.release_wake(&old_claim).await.unwrap());
    assert!(claim(&repo, &older, "retry").await.is_none());
    assert!(repo.batch(older.batch_id).await.unwrap().unwrap().sealed);
    assert!(!repo.batch(newer.batch_id).await.unwrap().unwrap().sealed);
    assert!(
        repo.pending_results(older.scope.workspace_id, older.scope.session_id, 10)
            .await
            .unwrap()
            .contains(&old_result)
    );

    // Newer user work retains both admission and automatic continuation rights.
    let extra_task = task(&newer);
    assert!(repo.register_task(&extra_task).await.unwrap());
    repo.finish_task(&result(&extra_task)).await.unwrap();
    repo.seal_batch(newer.batch_id).await.unwrap();
    let new_claim = claim(&repo, &newer, "new-owner").await.unwrap();
    assert!(repo.commit_wake(&new_claim).await.unwrap());
}

#[tokio::test]
async fn waiting_for_user_suspends_batch_and_stale_epoch_cannot_claim_it() {
    let store = RuntimeStore::in_memory().await.unwrap();
    let repo = store.background();
    let batch = batch();
    repo.create_batch(&batch).await.unwrap();
    let task = task(&batch);
    repo.register_task(&task).await.unwrap();
    repo.set_task_status(
        task.background_task_id,
        BackgroundTaskStatus::WaitingForUser,
    )
    .await
    .unwrap();
    repo.seal_batch(batch.batch_id).await.unwrap();
    assert!(claim(&repo, &batch, "waiting").await.is_none());
    repo.start_task(task.background_task_id).await.unwrap();
    repo.finish_task(&result(&task)).await.unwrap();
    assert!(
        repo.claim_batch_wake(
            batch.batch_id,
            batch.generation + 1,
            "stale",
            Utc::now(),
            Duration::seconds(30)
        )
        .await
        .unwrap()
        .is_none()
    );
    assert!(claim(&repo, &batch, "current").await.is_some());
}

#[tokio::test]
async fn persisted_handoff_is_delivered_once_and_recovery_preserves_user_control() {
    let store = RuntimeStore::in_memory().await.unwrap();
    let repo = store.background();
    let batch = batch();
    repo.create_batch(&batch).await.unwrap();
    let task = task(&batch);
    repo.register_task(&task).await.unwrap();
    let mut handoff = result(&task);
    handoff.status = BackgroundTaskStatus::WaitingForUser;
    handoff.output.status = ToolResultStatus::Blocked;
    assert!(repo.finish_task(&handoff).await.unwrap());
    assert!(!repo.finish_task(&handoff).await.unwrap());
    assert!(!repo.start_task(task.background_task_id).await.unwrap());
    repo.seal_batch(batch.batch_id).await.unwrap();
    assert!(claim(&repo, &batch, "paused").await.is_none());
    assert_eq!(
        repo.recover_abandoned(batch.scope.workspace_id, &[batch.scope.task_id])
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        repo.result_for_task(task.background_task_id).await.unwrap(),
        Some(handoff)
    );
    assert_eq!(
        repo.tasks(batch.batch_id).await.unwrap()[0].status,
        BackgroundTaskStatus::WaitingForUser
    );
}

#[tokio::test]
async fn exact_batch_claim_does_not_take_another_parent_with_same_generation() {
    let store = RuntimeStore::in_memory().await.unwrap();
    let repo = store.background();
    let first = batch();
    let mut second = first.clone();
    second.batch_id = BackgroundBatchId::new();
    second.scope.task_id = TaskId::new();
    for batch in [&first, &second] {
        repo.create_batch(batch).await.unwrap();
        let task = task(batch);
        repo.register_task(&task).await.unwrap();
        repo.finish_task(&result(&task)).await.unwrap();
        repo.seal_batch(batch.batch_id).await.unwrap();
    }
    let lease = claim(&repo, &second, "second").await.unwrap();
    assert_eq!(lease.batch.batch_id, second.batch_id);
    assert!(claim(&repo, &first, "first").await.is_none());
    repo.release_wake(&lease).await.unwrap();
    assert!(claim(&repo, &first, "first").await.is_some());
}

#[tokio::test]
async fn recovery_is_scoped_and_never_replays_or_wakes_uncertain_effects() {
    let store = RuntimeStore::in_memory().await.unwrap();
    let repo = store.background();
    let old = batch();
    let mut live = old.clone();
    live.batch_id = BackgroundBatchId::new();
    live.scope.task_id = TaskId::new();
    let old_task = task(&old);
    let live_task = task(&live);
    for (batch, task) in [(&old, &old_task), (&live, &live_task)] {
        repo.create_batch(batch).await.unwrap();
        repo.register_task(task).await.unwrap();
        repo.start_task(task.background_task_id).await.unwrap();
        repo.seal_batch(batch.batch_id).await.unwrap();
    }
    assert_eq!(
        repo.recover_abandoned(old.scope.workspace_id, &[old.scope.task_id])
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        repo.recover_abandoned(old.scope.workspace_id, &[old.scope.task_id])
            .await
            .unwrap(),
        0
    );
    let recovered = repo
        .result_for_task(old_task.background_task_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recovered.status, BackgroundTaskStatus::OutcomeUnknown);
    assert_ne!(recovered.output.status, ToolResultStatus::Ok);
    assert!(claim(&repo, &old, "restart").await.is_none());
    assert_eq!(
        repo.tasks(live.batch_id).await.unwrap()[0].status,
        BackgroundTaskStatus::Running
    );
    assert!(!repo.finish_task(&result(&old_task)).await.unwrap());
}

#[tokio::test]
async fn closed_batch_scope_and_output_bounds_are_enforced_without_losing_task() {
    let store = RuntimeStore::in_memory().await.unwrap();
    let repo = store.background();
    let batch = batch();
    repo.create_batch(&batch).await.unwrap();
    let task = task(&batch);
    repo.register_task(&task).await.unwrap();
    assert!(!repo.register_task(&task).await.unwrap());
    let mut wrong = result(&task);
    wrong.scope.session_id = SessionId::new();
    assert!(repo.finish_task(&wrong).await.is_err());
    let mut large = result(&task);
    large.summary = "x".repeat(MAX_BACKGROUND_SUMMARY_BYTES + 1);
    assert!(repo.finish_task(&large).await.is_err());
    assert!(
        repo.result_for_task(task.background_task_id)
            .await
            .unwrap()
            .is_none()
    );
    repo.seal_batch(batch.batch_id).await.unwrap();
    let mut extra = task.clone();
    extra.background_task_id = BackgroundTaskId::new();
    extra.tool_call_id = ToolCallId::new();
    assert!(repo.register_task(&extra).await.is_err());
    assert!(repo.finish_task(&result(&task)).await.unwrap());
}

#[tokio::test]
async fn stopped_batch_retains_late_result_without_reopening_execution() {
    let store = RuntimeStore::in_memory().await.unwrap();
    let repo = store.background();
    let batch = batch();
    repo.create_batch(&batch).await.unwrap();
    let task = task(&batch);
    repo.register_task(&task).await.unwrap();
    repo.start_task(task.background_task_id).await.unwrap();
    repo.suppress_session(batch.scope.workspace_id, batch.scope.session_id)
        .await
        .unwrap();
    assert!(repo.batch(batch.batch_id).await.unwrap().unwrap().sealed);
    assert!(!repo.start_task(task.background_task_id).await.unwrap());
    let output = result(&task);
    repo.finish_task(&output).await.unwrap();
    assert_eq!(
        repo.pending_results(batch.scope.workspace_id, batch.scope.session_id, 1)
            .await
            .unwrap(),
        vec![output.clone()]
    );
    assert!(claim(&repo, &batch, "late").await.is_none());
    assert_eq!(
        repo.acknowledge_results(
            WorkspaceId::new(),
            batch.scope.session_id,
            &[output.result_id],
            TurnId::new()
        )
        .await
        .unwrap(),
        0
    );
}

#[tokio::test]
async fn expired_wake_owner_cannot_commit_or_release_successor_claim() {
    let store = RuntimeStore::in_memory().await.unwrap();
    let repo = store.background();
    let batch = batch();
    repo.create_batch(&batch).await.unwrap();
    let task = task(&batch);
    repo.register_task(&task).await.unwrap();
    repo.finish_task(&result(&task)).await.unwrap();
    repo.seal_batch(batch.batch_id).await.unwrap();
    let old = repo
        .claim_batch_wake(
            batch.batch_id,
            batch.generation,
            "old",
            Utc::now() - Duration::minutes(1),
            Duration::seconds(1),
        )
        .await
        .unwrap()
        .unwrap();
    assert!(!repo.commit_wake(&old).await.unwrap());
    let current = claim(&repo, &batch, "current").await.unwrap();
    assert!(!repo.release_wake(&old).await.unwrap());
    assert!(!repo.commit_wake(&old).await.unwrap());
    assert!(repo.commit_wake(&current).await.unwrap());
}
