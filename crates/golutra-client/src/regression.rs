//! Baseline/candidate 隔离执行编排；`golutra-eval` 只保存事实和执行纯比较。

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use base64::Engine;
use futures_util::{FutureExt, future::BoxFuture};
use golutra_core::{
    ActorKind, EvaluationPartitionKind, RegressionCampaign, RegressionCampaignId,
    RegressionExecution, RegressionExecutionId, RegressionExecutionRole, RegressionExecutionStatus,
    TaskStatus, TraceView, VerificationResult,
};
use golutra_eval::{FrozenCandidatePatch, RegressionResult};
use golutra_protocol::{SessionCommand, SessionCommandKind, TaskTraceRequest};
use serde::{Deserialize, Serialize};
use walkdir::{DirEntry, WalkDir};

use super::*;

const REGRESSION_EXECUTION_TIMEOUT_SECS: u64 = 30;
const MAX_REGRESSION_CASES: usize = 32;
const MAX_REGRESSION_TRACE_BUNDLE_BYTES: usize = 64 * 1024 * 1024;
const MAX_CANDIDATE_FILES: usize = 64;
const MAX_CANDIDATE_FILE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone)]
struct RegressionCaseInput {
    case_ref: String,
    objective: String,
    partition: EvaluationPartitionKind,
}

#[derive(Debug, Clone)]
struct CandidateFile {
    relative_path: PathBuf,
    content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CandidatePatchBundle {
    format: String,
    files: BTreeMap<String, String>,
}

struct RegressionRoleInput<'a> {
    parent_session_id: SessionId,
    case_ref: &'a str,
    role: RegressionExecutionRole,
    home: &'a Path,
    workspace: &'a Path,
    objective: &'a str,
    partition: EvaluationPartitionKind,
    provider_variant: &'a str,
    seed: u64,
}

impl RuntimeHost {
    pub(crate) async fn run_regression_campaign(
        &self,
        parent_session_id: SessionId,
        command: &SessionCommand,
        candidate_id: &str,
    ) -> Result<RegressionResult, ClientError> {
        let evaluation_store = self.evaluation_store.clone();
        let state = run_blocking(move || evaluation_store.snapshot()).await??;
        let candidate = state
            .automation_candidates
            .iter()
            .find(|candidate| candidate.id == candidate_id)
            .cloned()
            .ok_or_else(|| EvaluationError::CandidateNotFound(candidate_id.to_owned()))?;
        let source_case = state
            .cases
            .iter()
            .find(|case| case.source_task_id == Some(candidate.source_task_id));
        let objective_override = command
            .payload
            .get("objective")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(ToOwned::to_owned);
        let (candidate_files, frozen_patch) = self
            .resolve_frozen_candidate_patch(
                parent_session_id,
                candidate.source_task_id,
                candidate_id,
                &command.payload,
            )
            .await?;
        let candidate_digest = frozen_patch.digest.clone();
        let mut case_refs = command
            .payload
            .get("case_refs")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>()
            })
            .filter(|values| !values.is_empty())
            .unwrap_or_else(|| {
                vec![source_case.map_or_else(
                    || format!("source-task:{}", candidate.source_task_id),
                    |case| case.case_id.clone(),
                )]
            });
        let mut seen_case_refs = std::collections::HashSet::new();
        case_refs.retain(|case_ref| seen_case_refs.insert(case_ref.clone()));
        if case_refs.len() > MAX_REGRESSION_CASES {
            return Err(ClientError::Evaluation(EvaluationError::InvalidBenchmark(
                format!("regression campaign exceeds {MAX_REGRESSION_CASES} cases"),
            )));
        }
        let regression_cases = case_refs
            .iter()
            .map(|case_ref| {
                let case = state
                    .cases
                    .iter()
                    .find(|case| case.case_id == *case_ref)
                    .ok_or_else(|| {
                        ClientError::Evaluation(EvaluationError::InvalidBenchmark(format!(
                            "regression case `{case_ref}` is not present in the durable evaluation store"
                        )))
                    })?;
                let objective = if case_refs.len() == 1 {
                    objective_override
                        .clone()
                        .unwrap_or_else(|| case.objective.clone())
                } else {
                    case.objective.clone()
                };
                Ok(RegressionCaseInput {
                    case_ref: case_ref.clone(),
                    objective,
                    partition: evaluation_case_partition(case),
                })
            })
            .collect::<Result<Vec<_>, ClientError>>()?;
        let provider_matrix = string_array_payload(&command.payload, "provider_matrix")
            .unwrap_or_else(|| vec!["isolated-mock".to_owned()]);
        if !provider_matrix
            .iter()
            .any(|provider| provider == "isolated-mock")
        {
            return Err(ClientError::TaskExecution(
                "regression campaign must include isolated-mock for the executable local baseline/candidate pair"
                    .to_owned(),
            ));
        }
        let seeds = u64_array_payload(&command.payload, "seeds").unwrap_or_else(|| vec![0]);
        let mut required_partitions = partition_array_payload(&command.payload)?
            .unwrap_or_else(|| regression_cases.iter().map(|case| case.partition).collect());
        required_partitions.sort();
        required_partitions.dedup();
        let case_partitions = regression_cases
            .iter()
            .map(|case| (case.case_ref.clone(), case.partition))
            .collect::<BTreeMap<_, _>>();
        let mut campaign = RegressionCampaign {
            campaign_id: RegressionCampaignId::new(),
            candidate_id: candidate_id.to_owned(),
            candidate_digest,
            candidate_artifact_ref: Some(frozen_patch.artifact_ref),
            baseline_version: env!("CARGO_PKG_VERSION").to_owned(),
            environment_recipe: "isolated-runtime-host/mock-provider/v1".to_owned(),
            case_refs: regression_cases
                .iter()
                .map(|case| case.case_ref.clone())
                .collect(),
            case_partitions,
            required_partitions,
            replay_modes: vec!["live_execution".to_owned()],
            provider_matrix,
            seeds: seeds.clone(),
            minimum_trusted_external_pairs: command
                .payload
                .get("minimum_trusted_external_pairs")
                .or_else(|| command.payload.get("minimum_trusted_external_evaluations"))
                .and_then(Value::as_u64)
                .map_or(0, |value| u32::try_from(value).unwrap_or(u32::MAX)),
            resource_budget: format!("timeout={}s", REGRESSION_EXECUTION_TIMEOUT_SECS),
            hard_gates: vec![
                "paired_task_trace".to_owned(),
                "verification_pass".to_owned(),
                "workspace_isolation".to_owned(),
            ],
            created_at: chrono::Utc::now(),
            started_at: Some(chrono::Utc::now()),
            completed_at: None,
        };
        let evaluation_store = self.evaluation_store.clone();
        let campaign_to_store = campaign.clone();
        run_blocking(move || evaluation_store.record_regression_campaign(campaign_to_store))
            .await??;
        self.record_event(host_event(
            self.next_sequence_no(),
            parent_session_id,
            Some(candidate.source_task_id),
            RuntimeEventType::RegressionCampaignStarted,
            RuntimeEventSource::Evaluator,
            json!({
                "summary": format!("regression campaign {} started", campaign.campaign_id),
                "campaign": campaign,
                "command_id": command.command_id,
            }),
        ))
        .await?;

        let isolation_root =
            tempfile::tempdir().map_err(|error| ClientError::Io(error.to_string()))?;
        for (case_index, case) in regression_cases.iter().enumerate() {
            let case_root = isolation_root.path().join(format!("case-{case_index}"));
            let baseline_workspace = case_root.join("baseline-workspace");
            let candidate_workspace = case_root.join("candidate-workspace");
            fs::create_dir_all(&baseline_workspace)
                .and_then(|_| fs::create_dir_all(&candidate_workspace))
                .map_err(|error| ClientError::Io(error.to_string()))?;
            if let Some(source) = self.workspace_root() {
                copy_workspace_snapshot(source, &baseline_workspace)?;
                copy_workspace_snapshot(source, &candidate_workspace)?;
            }
            apply_candidate_file_set(&candidate_workspace, &candidate_files)?;
            if workspace_digest(&baseline_workspace)? == workspace_digest(&candidate_workspace)? {
                return Err(ClientError::TaskExecution(format!(
                    "candidate patch is a no-op for regression case {}",
                    case.case_ref
                )));
            }

            let baseline_home = case_root.join("baseline-home");
            let candidate_home = case_root.join("candidate-home");
            for seed in &seeds {
                let baseline = self
                    .execute_regression_role(
                        &campaign,
                        RegressionRoleInput {
                            parent_session_id,
                            case_ref: &case.case_ref,
                            role: RegressionExecutionRole::Baseline,
                            home: &baseline_home,
                            workspace: &baseline_workspace,
                            objective: &case.objective,
                            partition: case.partition,
                            provider_variant: "isolated-mock",
                            seed: *seed,
                        },
                    )
                    .await?;
                let candidate_execution = self
                    .execute_regression_role(
                        &campaign,
                        RegressionRoleInput {
                            parent_session_id,
                            case_ref: &case.case_ref,
                            role: RegressionExecutionRole::Candidate,
                            home: &candidate_home,
                            workspace: &candidate_workspace,
                            objective: &case.objective,
                            partition: case.partition,
                            provider_variant: "isolated-mock",
                            seed: *seed,
                        },
                    )
                    .await?;
                for execution in [baseline, candidate_execution] {
                    let evaluation_store = self.evaluation_store.clone();
                    let stored = execution.clone();
                    run_blocking(move || evaluation_store.record_regression_execution(stored))
                        .await??;
                    self.record_event(host_event(
                        self.next_sequence_no(),
                        parent_session_id,
                        Some(candidate.source_task_id),
                        RuntimeEventType::RegressionExecutionCompleted,
                        RuntimeEventSource::Evaluator,
                        json!({
                            "summary": format!(
                                "{} {:?} regression execution finished as {:?}",
                                execution.case_ref, execution.role, execution.status
                            ),
                            "execution": execution,
                            "campaign_id": campaign.campaign_id,
                        }),
                    ))
                    .await?;
                }
            }
        }
        campaign.completed_at = Some(chrono::Utc::now());
        let evaluation_store = self.evaluation_store.clone();
        run_blocking(move || evaluation_store.record_regression_campaign(campaign)).await??;
        let evaluation_store = self.evaluation_store.clone();
        let candidate_id = candidate_id.to_owned();
        let regression =
            run_blocking(move || evaluation_store.run_regression(&candidate_id)).await??;
        Ok(regression)
    }

    pub(crate) fn automatically_process_improvement_candidate<'a>(
        &'a self,
        session_id: SessionId,
        candidate_id: &'a str,
    ) -> BoxFuture<'a, Result<(), ClientError>> {
        async move {
            let evaluation_store = self.evaluation_store.clone();
            let state = run_blocking(move || evaluation_store.snapshot()).await??;
            let Some(candidate) = state
                .automation_candidates
                .iter()
                .find(|candidate| candidate.id == candidate_id)
                .cloned()
            else {
                return Ok(());
            };
            if candidate.kind != golutra_eval::AutomationCandidateKind::RuntimeChange {
                return Ok(());
            }
            let frozen_patch = state
                .frozen_candidate_patches
                .iter()
                .find(|patch| patch.candidate_id == candidate_id)
                .cloned();
            let regression = if candidate.status == CandidateStatus::Proposed {
                let command = SessionCommand {
                    command_id: CommandId::new(),
                    session_id: Some(session_id),
                    kind: SessionCommandKind::RunRegression,
                    idempotency_key: format!("automatic-regression:{candidate_id}"),
                    actor: Actor {
                        kind: ActorKind::Runtime,
                        id: "automatic-improvement-dispatcher".to_owned(),
                    },
                    payload: json!({
                        "candidate_id": candidate_id,
                        "candidate_artifact_ref": frozen_patch.as_ref().map(|patch| patch.artifact_ref),
                    }),
                    timestamp: chrono::Utc::now(),
                };
                if frozen_patch.is_some() {
                    match self
                        .run_regression_campaign(session_id, &command, candidate_id)
                        .await
                    {
                        Ok(regression) => regression,
                        Err(error) => {
                            let evaluation_store = self.evaluation_store.clone();
                            let reason =
                                format!("automatic isolated regression could not run: {error}");
                            let candidate_id = candidate_id.to_owned();
                            run_blocking(move || {
                                evaluation_store
                                    .record_blocked_regression(&candidate_id, &reason)
                            })
                            .await??
                        }
                    }
                } else {
                    let evaluation_store = self.evaluation_store.clone();
                    let candidate_id = candidate_id.to_owned();
                    run_blocking(move || {
                        evaluation_store.record_blocked_regression(
                            &candidate_id,
                            "candidate has no immutable candidate_patch_set artifact; generate and freeze an executable patch before regression",
                        )
                    })
                    .await??
                }
            } else {
                let Some(regression) = state
                    .regressions
                    .iter()
                    .rev()
                    .find(|regression| regression.candidate_id == candidate_id)
                    .cloned()
                else {
                    return Ok(());
                };
                regression
            };
            let evaluation_store = self.evaluation_store.clone();
            let state = run_blocking(move || evaluation_store.snapshot()).await??;
            let decision = if let Some(decision) = state
                .promotion_decisions
                .iter()
                .rev()
                .find(|decision| decision.candidate_id == candidate_id)
                .cloned()
            {
                decision
            } else {
                let evaluation_store = self.evaluation_store.clone();
                let candidate_id_owned = candidate_id.to_owned();
                run_blocking(move || {
                    evaluation_store.decide_after_regression(&candidate_id_owned)
                })
                .await??
            };
            self.record_automatic_improvement_events(
                session_id,
                candidate.source_task_id,
                &regression,
                &decision,
            )
            .await
        }
        .boxed()
    }

    async fn record_automatic_improvement_events(
        &self,
        session_id: SessionId,
        source_task_id: TaskId,
        regression: &RegressionResult,
        decision: &golutra_eval::PromotionDecision,
    ) -> Result<(), ClientError> {
        let events = self
            .repositories
            .events
            .load(session_id, Some(source_task_id), None)
            .await?;
        let has_regression = events.iter().any(|event| {
            event.event_type == RuntimeEventType::RegressionCompleted
                && event.payload["record"]["regression_id"] == regression.regression_id
        });
        if !has_regression {
            self.record_event(host_event(
                self.next_sequence_no(),
                session_id,
                Some(source_task_id),
                RuntimeEventType::RegressionCompleted,
                RuntimeEventSource::Evaluator,
                json!({
                    "summary": format!(
                        "candidate {} automatic regression settled",
                        regression.candidate_id
                    ),
                    "record": regression,
                    "automatic": true,
                }),
            ))
            .await?;
        }
        let has_decision = events.iter().any(|event| {
            event.event_type == RuntimeEventType::PromotionDecided
                && event.payload["record"]["decision_id"] == decision.decision_id
        });
        if !has_decision {
            self.record_event(host_event(
                self.next_sequence_no(),
                session_id,
                Some(source_task_id),
                RuntimeEventType::PromotionDecided,
                RuntimeEventSource::Evaluator,
                json!({
                    "summary": format!(
                        "candidate {} automatic promotion decision: {:?}",
                        decision.candidate_id, decision.decision
                    ),
                    "record": decision,
                    "automatic": true,
                }),
            ))
            .await?;
        }
        Ok(())
    }

    async fn resolve_frozen_candidate_patch(
        &self,
        session_id: SessionId,
        source_task_id: TaskId,
        candidate_id: &str,
        payload: &Value,
    ) -> Result<(Vec<CandidateFile>, FrozenCandidatePatch), ClientError> {
        let supplied_files = candidate_files_from_payload(payload)?;
        let evaluation_store = self.evaluation_store.clone();
        let candidate_id_owned = candidate_id.to_owned();
        let existing = run_blocking(move || {
            evaluation_store.snapshot().map(|state| {
                state
                    .frozen_candidate_patches
                    .into_iter()
                    .find(|patch| patch.candidate_id == candidate_id_owned)
            })
        })
        .await??;
        if let Some(patch) = existing {
            let files = self.load_frozen_candidate_patch(session_id, &patch).await?;
            if !supplied_files.is_empty() && candidate_files_digest(&supplied_files) != patch.digest
            {
                return Err(ClientError::TaskExecution(
                    "candidate_files do not match the immutable frozen candidate patch".to_owned(),
                ));
            }
            validate_declared_candidate_digest(payload, &patch.digest)?;
            validate_declared_candidate_artifact(payload, patch.artifact_ref)?;
            self.record_candidate_patch_event_if_missing(session_id, &patch)
                .await?;
            return Ok((files, patch));
        }
        if supplied_files.is_empty() {
            return Err(ClientError::TaskExecution(
                "regression candidate has no frozen candidate_patch_set artifact or candidate_files"
                    .to_owned(),
            ));
        }
        let patch = self
            .freeze_candidate_patch(session_id, source_task_id, candidate_id, &supplied_files)
            .await?;
        validate_declared_candidate_digest(payload, &patch.digest)?;
        validate_declared_candidate_artifact(payload, patch.artifact_ref)?;
        Ok((supplied_files, patch))
    }

    async fn freeze_candidate_patch(
        &self,
        session_id: SessionId,
        source_task_id: TaskId,
        candidate_id: &str,
        files: &[CandidateFile],
    ) -> Result<FrozenCandidatePatch, ClientError> {
        let bundle = CandidatePatchBundle {
            format: "golutra.candidate-patch.v1".to_owned(),
            files: files
                .iter()
                .map(|file| {
                    (
                        file.relative_path.to_string_lossy().into_owned(),
                        file.content.clone(),
                    )
                })
                .collect(),
        };
        let bytes = serde_json::to_vec(&bundle)?;
        let artifact_id = ArtifactId::new();
        let event_id = EventId::new();
        let checksum = format!("sha256:{:x}", Sha256::digest(&bytes));
        let artifact = ArtifactRecord {
            artifact_id,
            session_id,
            turn_id: None,
            tool_call_id: None,
            artifact_type: "candidate_patch_set".to_owned(),
            uri: format!("artifact://candidate-patch/{candidate_id}/{artifact_id}"),
            checksum,
            size_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            created_at: chrono::Utc::now(),
            producer: "improvement-candidate-dispatcher".to_owned(),
            redaction_status: RedactionStatus::Raw,
            retention_policy: "governance_evidence".to_owned(),
            provenance_refs: vec![event_id],
        };
        self.repositories.artifacts.store(&artifact, &bytes).await?;
        let patch = FrozenCandidatePatch {
            candidate_id: candidate_id.to_owned(),
            source_task_id,
            artifact_ref: artifact_id,
            digest: candidate_files_digest(files),
            format: bundle.format,
            file_count: u32::try_from(files.len()).unwrap_or(u32::MAX),
            total_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            frozen_at: chrono::Utc::now(),
        };
        let evaluation_store = self.evaluation_store.clone();
        let stored_patch = patch.clone();
        run_blocking(move || evaluation_store.record_frozen_candidate_patch(stored_patch))
            .await??;
        self.record_candidate_patch_event_if_missing(session_id, &patch)
            .await?;
        Ok(patch)
    }

    async fn record_candidate_patch_event_if_missing(
        &self,
        session_id: SessionId,
        patch: &FrozenCandidatePatch,
    ) -> Result<(), ClientError> {
        let events = self
            .repositories
            .events
            .load(session_id, Some(patch.source_task_id), None)
            .await?;
        if events.iter().any(|event| {
            event.event_type == RuntimeEventType::CandidatePatchFrozen
                && event.payload_ref == Some(patch.artifact_ref)
                && event.payload["record"]["candidate_id"] == patch.candidate_id
                && event.payload["record"]["artifact_ref"] == patch.artifact_ref.to_string()
                && event.payload["record"]["digest"] == patch.digest
        }) {
            return Ok(());
        }
        let artifact = self
            .repositories
            .artifacts
            .get(patch.artifact_ref)
            .await?
            .ok_or_else(|| {
                ClientError::TaskExecution("frozen candidate patch artifact is missing".to_owned())
            })?;
        let event_id = artifact.provenance_refs.first().copied().ok_or_else(|| {
            ClientError::TaskExecution(
                "frozen candidate patch artifact has no canonical event provenance".to_owned(),
            )
        })?;
        let mut event = host_event(
            self.next_sequence_no(),
            session_id,
            Some(patch.source_task_id),
            RuntimeEventType::CandidatePatchFrozen,
            RuntimeEventSource::Evaluator,
            json!({
                "summary": format!(
                    "candidate {} executable patch frozen",
                    patch.candidate_id
                ),
                "record": patch,
            }),
        );
        event.id = event_id;
        event.payload_ref = Some(patch.artifact_ref);
        self.record_event(event).await
    }

    async fn load_frozen_candidate_patch(
        &self,
        session_id: SessionId,
        patch: &FrozenCandidatePatch,
    ) -> Result<Vec<CandidateFile>, ClientError> {
        let artifact = self
            .repositories
            .artifacts
            .get(patch.artifact_ref)
            .await?
            .ok_or_else(|| {
                ClientError::TaskExecution("frozen candidate patch artifact is missing".to_owned())
            })?;
        if artifact.session_id != session_id || artifact.artifact_type != "candidate_patch_set" {
            return Err(ClientError::TaskExecution(
                "frozen candidate patch artifact belongs to another session or type".to_owned(),
            ));
        }
        let bytes = self
            .repositories
            .artifacts
            .bytes(patch.artifact_ref)
            .await?
            .ok_or_else(|| {
                ClientError::TaskExecution(
                    "frozen candidate patch artifact content is unavailable".to_owned(),
                )
            })?;
        let bundle: CandidatePatchBundle = serde_json::from_slice(&bytes)?;
        if bundle.format != patch.format || bundle.format != "golutra.candidate-patch.v1" {
            return Err(ClientError::TaskExecution(
                "frozen candidate patch format is unsupported".to_owned(),
            ));
        }
        let files = candidate_files_from_map(&bundle.files)?;
        if candidate_files_digest(&files) != patch.digest
            || u32::try_from(files.len()).unwrap_or(u32::MAX) != patch.file_count
            || u64::try_from(bytes.len()).unwrap_or(u64::MAX) != patch.total_bytes
        {
            return Err(ClientError::TaskExecution(
                "frozen candidate patch digest or file count does not match its record".to_owned(),
            ));
        }
        Ok(files)
    }

    async fn execute_regression_role(
        &self,
        campaign: &RegressionCampaign,
        input: RegressionRoleInput<'_>,
    ) -> Result<RegressionExecution, ClientError> {
        let session_id = SessionId::new();
        let child = self
            .isolated_regression_host(input.home, input.workspace, session_id)
            .await?;
        let command = SessionCommand {
            command_id: CommandId::new(),
            session_id: Some(session_id),
            kind: SessionCommandKind::Prompt,
            idempotency_key: format!(
                "regression:{}:{role:?}:{}:{}:{:x}",
                campaign.campaign_id,
                input.provider_variant,
                input.seed,
                Sha256::digest(input.case_ref.as_bytes()),
                role = input.role,
            ),
            actor: Actor {
                kind: ActorKind::Runtime,
                id: format!("regression:{}", campaign.campaign_id),
            },
            payload: json!({
                "prompt": input.objective,
                "regression_role": input.role,
                "regression_case_ref": input.case_ref,
                "regression_partition": input.partition,
                "regression_provider_variant": input.provider_variant,
                "regression_seed": input.seed,
            }),
            timestamp: chrono::Utc::now(),
        };
        // 子 runtime 复用统一 command 协议；此处是递归协议边界，显式装箱避免 async future 无限展开。
        let ack = Box::pin(child.clone().handle_command(command)).await?;
        if ack.accepted {
            let _ = tokio::time::timeout(
                Duration::from_secs(REGRESSION_EXECUTION_TIMEOUT_SECS),
                child.wait_for_finishing_task_control(session_id),
            )
            .await;
        }
        let state = child
            .repositories
            .projections
            .state(session_id, None)
            .await?;
        let task_id = state.active_task_id;
        if let Some(task_id) = task_id {
            child.wait_for_deep_task_evaluation(task_id).await;
        }
        let trace = match task_id {
            Some(task_id) => TaskTraceService::new(child.clone())
                .read_complete(TaskTraceRequest {
                    session_id,
                    task_id,
                    view: TraceView::Full,
                    cursor: None,
                    limit: 512,
                    wait_for_evaluation: true,
                })
                .await
                .ok(),
            None => None,
        };
        let verification = trace.as_ref().and_then(|trace| trace.verification.as_ref());
        let verification_ref = verification.map(|record| record.verification_id);
        let task_trace_ref = match trace.as_ref() {
            Some(trace) if trace.integrity.complete => {
                self.persist_regression_trace_bundle(
                    input.parent_session_id,
                    &child,
                    campaign,
                    input.case_ref,
                    input.role,
                    trace,
                )
                .await?
            }
            _ => None,
        };
        let succeeded = ack.accepted
            && state.task_status == TaskStatus::Completed
            && verification.is_some_and(|record| record.result == VerificationResult::Pass)
            && task_trace_ref.is_some();
        let workspace_snapshot_digest = workspace_digest(input.workspace)?;
        Ok(RegressionExecution {
            execution_id: RegressionExecutionId::new(),
            campaign_id: campaign.campaign_id,
            case_ref: input.case_ref.to_owned(),
            partition: input.partition,
            provider_variant: input.provider_variant.to_owned(),
            seed: input.seed,
            role: input.role,
            runtime_version: env!("CARGO_PKG_VERSION").to_owned(),
            workspace_snapshot_digest,
            task_trace_ref,
            verification_ref,
            cost_latency_ref: task_id.map(|task_id| format!("evaluation:{task_id}")),
            status: if succeeded {
                RegressionExecutionStatus::Succeeded
            } else if ack.accepted {
                RegressionExecutionStatus::Inconclusive
            } else {
                RegressionExecutionStatus::Failed
            },
        })
    }

    async fn persist_regression_trace_bundle(
        &self,
        parent_session_id: SessionId,
        child: &RuntimeHost,
        campaign: &RegressionCampaign,
        case_ref: &str,
        role: RegressionExecutionRole,
        trace: &TaskTracePage,
    ) -> Result<Option<String>, ClientError> {
        let mut artifact_blobs = Vec::with_capacity(trace.artifacts.len());
        for artifact in &trace.artifacts {
            let Some(bytes) = child
                .repositories
                .artifacts
                .bytes(artifact.artifact_id)
                .await?
            else {
                return Ok(None);
            };
            artifact_blobs.push(json!({
                "artifact": artifact,
                "content_base64": base64::engine::general_purpose::STANDARD.encode(bytes),
            }));
        }
        let bytes = serde_json::to_vec(&json!({
            "format": "golutra.regression-trace-bundle.v1",
            "campaign_id": campaign.campaign_id,
            "case_ref": case_ref,
            "role": role,
            "trace": trace,
            "artifact_blobs": artifact_blobs,
        }))?;
        if bytes.len() > MAX_REGRESSION_TRACE_BUNDLE_BYTES {
            return Ok(None);
        }
        let artifact_id = ArtifactId::new();
        let checksum = format!("sha256:{:x}", Sha256::digest(&bytes));
        let uri = format!("artifact://regression-trace/{artifact_id}");
        self.repositories
            .artifacts
            .store(
                &ArtifactRecord {
                    artifact_id,
                    session_id: parent_session_id,
                    turn_id: None,
                    tool_call_id: None,
                    artifact_type: "regression_trace_bundle".to_owned(),
                    uri: uri.clone(),
                    checksum: checksum.clone(),
                    size_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                    created_at: chrono::Utc::now(),
                    producer: "regression-service".to_owned(),
                    redaction_status: RedactionStatus::Redacted,
                    retention_policy: "governance_evidence".to_owned(),
                    provenance_refs: trace.events.iter().map(|event| event.id).collect(),
                },
                &bytes,
            )
            .await?;
        Ok(Some(format!("{uri}?checksum={checksum}")))
    }

    async fn isolated_regression_host(
        &self,
        home: &Path,
        workspace: &Path,
        session_id: SessionId,
    ) -> Result<Arc<RuntimeHost>, ClientError> {
        let paths = RuntimePaths::from_home_and_cwd(home, workspace)?;
        let store = RuntimeStore::connect_with_artifact_root(
            &paths.sqlite_url(),
            paths.artifacts_dir.clone(),
        )
        .await?;
        set_owner_only_file(&paths.runtime_db)?;
        RuntimeHost::from_store(
            store,
            Some(paths.cwd.clone()),
            RuntimeHostStorage::durable(paths.clone())?,
            paths.workspace_id(),
            session_id,
            ThreadId::new(),
            true,
            RuntimeExecutionOptions::isolated(),
        )
        .await
    }
}

fn copy_workspace_snapshot(source: &Path, destination: &Path) -> Result<(), ClientError> {
    let mut entries = WalkDir::new(source)
        .follow_links(false)
        .into_iter()
        .filter_entry(included_snapshot_entry)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| ClientError::Io(error.to_string()))?;
    entries.sort_by_key(|entry| entry.path().to_path_buf());
    for entry in entries.into_iter().skip(1) {
        let relative = entry
            .path()
            .strip_prefix(source)
            .map_err(|error| ClientError::Io(error.to_string()))?;
        let target = destination.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target).map_err(|error| ClientError::Io(error.to_string()))?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|error| ClientError::Io(error.to_string()))?;
            }
            fs::copy(entry.path(), &target).map_err(|error| ClientError::Io(error.to_string()))?;
        }
    }
    Ok(())
}

fn included_snapshot_entry(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    if entry.file_type().is_symlink() {
        return false;
    }
    !matches!(
        entry.file_name().to_str(),
        Some(".git" | ".golutra" | "target" | "node_modules")
    )
}

fn evaluation_case_partition(case: &golutra_eval::EvaluationCase) -> EvaluationPartitionKind {
    if let Some(partition) = case
        .tags
        .iter()
        .find_map(|tag| tag.strip_prefix("partition:"))
        .and_then(parse_partition)
    {
        return partition;
    }
    parse_partition(&case.source).unwrap_or_default()
}

fn parse_partition(value: &str) -> Option<EvaluationPartitionKind> {
    match value.trim().to_ascii_lowercase().as_str() {
        "source" => Some(EvaluationPartitionKind::Source),
        "historical" => Some(EvaluationPartitionKind::Historical),
        "generated" => Some(EvaluationPartitionKind::Generated),
        "holdout" => Some(EvaluationPartitionKind::Holdout),
        "adversarial" => Some(EvaluationPartitionKind::Adversarial),
        _ => None,
    }
}

fn partition_array_payload(
    payload: &Value,
) -> Result<Option<Vec<EvaluationPartitionKind>>, ClientError> {
    let Some(values) = payload.get("required_partitions") else {
        return Ok(None);
    };
    let values = values.as_array().ok_or_else(|| {
        ClientError::TaskExecution("required_partitions must be a JSON array".to_owned())
    })?;
    let mut partitions = values
        .iter()
        .map(|value| {
            value.as_str().and_then(parse_partition).ok_or_else(|| {
                ClientError::TaskExecution(
                    "required_partitions contains an unknown partition".to_owned(),
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    partitions.sort();
    partitions.dedup();
    Ok((!partitions.is_empty()).then_some(partitions))
}

fn string_array_payload(payload: &Value, key: &str) -> Option<Vec<String>> {
    let mut values = payload
        .get(key)?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    (!values.is_empty()).then_some(values)
}

fn u64_array_payload(payload: &Value, key: &str) -> Option<Vec<u64>> {
    let mut values = payload
        .get(key)?
        .as_array()?
        .iter()
        .filter_map(Value::as_u64)
        .collect::<Vec<_>>();
    values.sort_unstable();
    values.dedup();
    (!values.is_empty()).then_some(values)
}

fn candidate_files_from_payload(payload: &Value) -> Result<Vec<CandidateFile>, ClientError> {
    let Some(files) = payload.get("candidate_files").and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    if files.len() > MAX_CANDIDATE_FILES {
        return Err(ClientError::TaskExecution(format!(
            "candidate_files exceeds {MAX_CANDIDATE_FILES} entries"
        )));
    }
    let mut candidate_files = Vec::with_capacity(files.len());
    for (relative, content) in files {
        let content = content.as_str().ok_or_else(|| {
            ClientError::TaskExecution("candidate_files values must be strings".to_owned())
        })?;
        candidate_files.push(candidate_file(relative, content)?);
    }
    candidate_files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(candidate_files)
}

fn candidate_files_from_map(
    files: &BTreeMap<String, String>,
) -> Result<Vec<CandidateFile>, ClientError> {
    if files.len() > MAX_CANDIDATE_FILES {
        return Err(ClientError::TaskExecution(format!(
            "candidate patch exceeds {MAX_CANDIDATE_FILES} entries"
        )));
    }
    files
        .iter()
        .map(|(relative, content)| candidate_file(relative, content))
        .collect()
}

fn candidate_file(relative: &str, content: &str) -> Result<CandidateFile, ClientError> {
    let relative = PathBuf::from(relative);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(ClientError::TaskExecution(
            "candidate file paths must stay inside the isolated workspace".to_owned(),
        ));
    }
    if let Some(reason) = sealed_candidate_path_reason(&relative) {
        return Err(ClientError::TaskExecution(format!(
            "candidate file `{}` modifies sealed control-plane state: {reason}",
            relative.display()
        )));
    }
    if content.len() > MAX_CANDIDATE_FILE_BYTES {
        return Err(ClientError::TaskExecution(format!(
            "candidate file exceeds {MAX_CANDIDATE_FILE_BYTES} bytes"
        )));
    }
    Ok(CandidateFile {
        relative_path: relative,
        content: content.to_owned(),
    })
}

fn validate_declared_candidate_digest(
    payload: &Value,
    candidate_digest: &str,
) -> Result<(), ClientError> {
    if let Some(declared_digest) = payload
        .get("candidate_digest")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        && declared_digest != candidate_digest
    {
        return Err(ClientError::TaskExecution(
            "declared candidate_digest does not match the immutable candidate patch".to_owned(),
        ));
    }
    Ok(())
}

fn validate_declared_candidate_artifact(
    payload: &Value,
    artifact_ref: ArtifactId,
) -> Result<(), ClientError> {
    let Some(declared) = payload.get("candidate_artifact_ref") else {
        return Ok(());
    };
    let declared = declared.as_str().ok_or_else(|| {
        ClientError::TaskExecution("candidate_artifact_ref must be an artifact id".to_owned())
    })?;
    if declared != artifact_ref.to_string() {
        return Err(ClientError::TaskExecution(
            "candidate_artifact_ref does not match the immutable candidate patch".to_owned(),
        ));
    }
    Ok(())
}

fn candidate_files_digest(files: &[CandidateFile]) -> String {
    let mut hasher = Sha256::new();
    for file in files {
        let relative = file.relative_path.to_string_lossy();
        hasher.update(
            u64::try_from(relative.len())
                .unwrap_or(u64::MAX)
                .to_le_bytes(),
        );
        hasher.update(relative.as_bytes());
        hasher.update(
            u64::try_from(file.content.len())
                .unwrap_or(u64::MAX)
                .to_le_bytes(),
        );
        hasher.update(file.content.as_bytes());
    }
    format!("sha256:{:x}", hasher.finalize())
}

fn apply_candidate_file_set(workspace: &Path, files: &[CandidateFile]) -> Result<(), ClientError> {
    for file in files {
        let target = workspace.join(&file.relative_path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| ClientError::Io(error.to_string()))?;
        }
        fs::write(target, &file.content).map_err(|error| ClientError::Io(error.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
fn apply_candidate_files(workspace: &Path, payload: &Value) -> Result<(), ClientError> {
    let files = candidate_files_from_payload(payload)?;
    apply_candidate_file_set(workspace, &files)
}

fn sealed_candidate_path_reason(relative: &Path) -> Option<&'static str> {
    let components = relative
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => {
                Some(value.to_string_lossy().to_ascii_lowercase())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let first = components.first().map(String::as_str);
    if matches!(
        first,
        Some(".git" | ".golutra" | ".github" | "target" | "node_modules")
    ) {
        return Some("repository, runtime-state, build, or CI metadata is immutable");
    }
    if components.first().is_some_and(|value| value == "crates")
        && components.get(1).is_some_and(|value| {
            matches!(
                value.as_str(),
                "golutra-eval" | "golutra-verify" | "golutra-policy" | "golutra-sandbox"
            )
        })
    {
        return Some("evaluator, verifier, policy, and sandbox crates are sealed");
    }
    if components.iter().any(|component| {
        [
            "hidden-test",
            "hidden_test",
            "promotion-gate",
            "promotion_gate",
            "stable-pointer",
            "stable_pointer",
            "signer",
            "signing-key",
            "signing_key",
        ]
        .iter()
        .any(|marker| component.contains(marker))
    }) {
        return Some("hidden evaluation, promotion, stable pointer, and signing state is sealed");
    }
    None
}

fn workspace_digest(workspace: &Path) -> Result<String, ClientError> {
    let mut files = WalkDir::new(workspace)
        .follow_links(false)
        .into_iter()
        .filter_entry(included_snapshot_entry)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| ClientError::Io(error.to_string()))?;
    files.retain(|entry| entry.file_type().is_file());

    files.sort_by_key(|entry| entry.path().to_path_buf());
    let mut digest = Sha256::new();
    for entry in files {
        let relative = entry
            .path()
            .strip_prefix(workspace)
            .map_err(|error| ClientError::Io(error.to_string()))?;
        digest.update(relative.to_string_lossy().as_bytes());
        digest.update(fs::read(entry.path()).map_err(|error| ClientError::Io(error.to_string()))?);
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn candidate_files_reject_control_plane_and_runtime_metadata_paths() {
        let workspace = tempdir().expect("workspace");
        for path in [
            ".git/config",
            ".golutra/runtime.json",
            ".github/workflows/evaluation.yml",
            "crates/golutra-eval/src/runner.rs",
            "crates/golutra-verify/src/lib.rs",
            "crates/golutra-policy/src/lib.rs",
            "crates/golutra-sandbox/src/lib.rs",
            "release/stable-pointer.json",
            "secrets/signer.json",
        ] {
            let error = apply_candidate_files(
                workspace.path(),
                &json!({"candidate_files": {path: "tampered"}}),
            )
            .expect_err("sealed path");
            assert!(error.to_string().contains("sealed control-plane"));
        }
    }

    #[test]
    fn candidate_files_allow_regular_workspace_changes() {
        let workspace = tempdir().expect("workspace");
        apply_candidate_files(
            workspace.path(),
            &json!({"candidate_files": {"src/feature.rs": "pub fn feature() {}"}}),
        )
        .expect("candidate files");

        assert_eq!(
            fs::read_to_string(workspace.path().join("src/feature.rs")).expect("candidate file"),
            "pub fn feature() {}"
        );
    }

    #[test]
    fn candidate_file_digest_is_canonical_and_content_sensitive() {
        let first = candidate_files_from_payload(&json!({
            "candidate_files": {"b.txt": "two", "a.txt": "one"}
        }))
        .expect("candidate files");
        let reordered = candidate_files_from_payload(&json!({
            "candidate_files": {"a.txt": "one", "b.txt": "two"}
        }))
        .expect("candidate files");
        let changed = candidate_files_from_payload(&json!({
            "candidate_files": {"a.txt": "changed", "b.txt": "two"}
        }))
        .expect("candidate files");

        assert_eq!(
            candidate_files_digest(&first),
            candidate_files_digest(&reordered)
        );
        assert_ne!(
            candidate_files_digest(&first),
            candidate_files_digest(&changed)
        );
    }

    #[tokio::test]
    async fn missing_candidate_patch_event_is_recovered_idempotently() {
        let application = RuntimeApplication::in_memory().await.expect("application");
        let host = application.host();
        let session_id = application.session_service().default_session_id();
        let source_task_id = TaskId::new();
        let artifact_id = ArtifactId::new();
        let event_id = EventId::new();
        let bytes = br#"{"format":"golutra.candidate-patch.v1","files":{"src/lib.rs":""}}"#;
        let artifact = ArtifactRecord {
            artifact_id,
            session_id,
            turn_id: None,
            tool_call_id: None,
            artifact_type: "candidate_patch_set".to_owned(),
            uri: format!("artifact://candidate-patch/test/{artifact_id}"),
            checksum: format!("sha256:{:x}", Sha256::digest(bytes)),
            size_bytes: u64::try_from(bytes.len()).expect("artifact size"),
            created_at: chrono::Utc::now(),
            producer: "test".to_owned(),
            redaction_status: RedactionStatus::Raw,
            retention_policy: "governance_evidence".to_owned(),
            provenance_refs: vec![event_id],
        };
        host.repositories
            .artifacts
            .store(&artifact, bytes)
            .await
            .expect("candidate artifact");
        let patch = FrozenCandidatePatch {
            candidate_id: "candidate-crash-recovery".to_owned(),
            source_task_id,
            artifact_ref: artifact_id,
            digest: "sha256:test".to_owned(),
            format: "golutra.candidate-patch.v1".to_owned(),
            file_count: 1,
            total_bytes: u64::try_from(bytes.len()).expect("patch size"),
            frozen_at: chrono::Utc::now(),
        };
        host.record_event(host_event(
            host.next_sequence_no(),
            session_id,
            Some(source_task_id),
            RuntimeEventType::CandidatePatchFrozen,
            RuntimeEventSource::Evaluator,
            json!({
                "record": {
                    "candidate_id": patch.candidate_id,
                    "artifact_ref": ArtifactId::new(),
                    "digest": "sha256:stale",
                }
            }),
        ))
        .await
        .expect("stale candidate event");

        host.record_candidate_patch_event_if_missing(session_id, &patch)
            .await
            .expect("recover event");
        host.record_candidate_patch_event_if_missing(session_id, &patch)
            .await
            .expect("idempotent recovery");

        let matching = host
            .repositories
            .events
            .load(session_id, Some(source_task_id), None)
            .await
            .expect("candidate events")
            .into_iter()
            .filter(|event| {
                event.event_type == RuntimeEventType::CandidatePatchFrozen
                    && event.payload_ref == Some(artifact_id)
            })
            .collect::<Vec<_>>();
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0].id, event_id);
        assert_eq!(matching[0].payload_ref, Some(artifact_id));
        assert_eq!(
            matching[0].payload["record"]["candidate_id"],
            patch.candidate_id
        );
    }
}
