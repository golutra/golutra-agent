//! 独立、受权限约束的自进化控制面。
//!
//! `EvolutionSupervisor` 只消费脱敏且完整的观察事实，负责候选生命周期和 release
//! 指针；它不参与普通 AgentLoop，也不会把候选工作区的权限扩展到 evaluator、签名或
//! stable pointer。每个 epoch 都有预算和终态，调用方必须显式推进下一阶段。

mod evaluation_runner;
mod model;
mod producer;
mod store;

pub use model::*;
pub use producer::{
    CandidateProducer, ExternalCommandProducer, InternalCommandProducer, StaticCandidateProducer,
};
pub use store::{
    SupervisorError, SupervisorPaths, SupervisorStore, candidate_tree_digest,
    validate_candidate_changes, validate_candidate_worktree, validate_target_path,
};

use std::{
    collections::HashSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use chrono::Utc;
use golutra_core::TraceView;
use golutra_protocol::{MINIMUM_RUNTIME_PROTOCOL_VERSION, RUNTIME_PROTOCOL_VERSION, TaskTracePage};
use golutra_release::{
    BuildReport, DeploymentPhase, ReleaseBuildRequest, ReleaseManifest, ReleasePointer,
    ReleaseStore, TrustedBuilder,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use store::validate_producer_worktree;
use uuid::Uuid;

const MAX_TEXT: usize = 2_048;
const MAX_OBSERVATION_REFS: usize = 128;
const MAX_PROVENANCE_FILE_BYTES: u64 = 16 * 1024 * 1024;

fn runtime_protocol_version_range() -> String {
    format!("{MINIMUM_RUNTIME_PROTOCOL_VERSION}..={RUNTIME_PROTOCOL_VERSION}")
}

#[derive(Debug, Clone)]
pub struct EvolutionSupervisor {
    store: SupervisorStore,
    release_store: ReleaseStore,
}

impl EvolutionSupervisor {
    pub fn new(
        supervisor_root: impl Into<PathBuf>,
        release_root: impl Into<PathBuf>,
    ) -> Result<Self, SupervisorError> {
        let supervisor = Self {
            store: SupervisorStore::new(supervisor_root)?,
            release_store: ReleaseStore::new(release_root)?,
        };
        supervisor.reconcile_deployment_state()?;
        Ok(supervisor)
    }

    #[must_use]
    pub fn store(&self) -> &SupervisorStore {
        &self.store
    }

    #[must_use]
    pub fn release_store(&self) -> &ReleaseStore {
        &self.release_store
    }

    fn validate_frozen_candidate_source(
        &self,
        candidate: &EvolutionCandidate,
    ) -> Result<(), SupervisorError> {
        let snapshot = self.store.snapshot()?;
        let parent_release_id = snapshot
            .epochs
            .iter()
            .find(|epoch| epoch.epoch_id == candidate.epoch_id)
            .and_then(|epoch| epoch.parent_release_id.as_deref())
            .ok_or_else(|| {
                SupervisorError::Integrity(
                    "frozen candidate has no parent release binding".to_owned(),
                )
            })?;
        self.ensure_current_stable(parent_release_id)?;
        let parent_source = self.release_store.release_source(parent_release_id)?;
        let actual_paths = validate_candidate_changes(
            &parent_source,
            &candidate.worktree,
            &candidate.target_paths,
        )?;
        if actual_paths != candidate.target_paths
            || candidate_tree_digest(&candidate.worktree)? != candidate.patch_digest
        {
            return Err(SupervisorError::Integrity(
                "candidate source changed after it was frozen".to_owned(),
            ));
        }
        Ok(())
    }

    fn ensure_current_stable(&self, expected_release_id: &str) -> Result<(), SupervisorError> {
        let stable = self.release_store.pointer("stable")?.ok_or_else(|| {
            SupervisorError::InvalidTransition(
                "evolution requires a bootstrapped stable release".to_owned(),
            )
        })?;
        if stable.release_id != expected_release_id {
            return Err(SupervisorError::GateRejected(format!(
                "candidate parent {expected_release_id} is stale relative to stable release {}",
                stable.release_id
            )));
        }
        Ok(())
    }

    fn reconcile_deployment_state(&self) -> Result<(), SupervisorError> {
        let snapshot = self.store.snapshot()?;
        if snapshot.candidates.is_empty() {
            return Ok(());
        }
        let records = self.release_store.verify_deployment_log()?;
        let preview = self
            .release_store
            .pointer("preview")?
            .map(|pointer| pointer.release_id);
        let canary = self
            .release_store
            .pointer("canary")?
            .map(|pointer| pointer.release_id);
        let stable = self
            .release_store
            .pointer("stable")?
            .map(|pointer| pointer.release_id);
        let mut updates = Vec::new();
        for candidate in &snapshot.candidates {
            let Some(release_id) = candidate.release_id.as_deref() else {
                continue;
            };
            let desired = if canary.as_deref() == Some(release_id) {
                Some((CandidateStatus::Canarying, EpochStatus::Canarying))
            } else if preview.as_deref() == Some(release_id) {
                Some((CandidateStatus::Previewing, EpochStatus::Previewing))
            } else if stable.as_deref() == Some(release_id) {
                Some((CandidateStatus::Promoted, EpochStatus::Promoted))
            } else if records.iter().rev().any(|record| {
                record.phase == DeploymentPhase::RolledBack
                    && record.previous_release_id.as_deref() == Some(release_id)
            }) {
                Some((CandidateStatus::RolledBack, EpochStatus::RolledBack))
            } else {
                None
            };
            if let Some((candidate_status, epoch_status)) = desired
                && candidate.status != candidate_status
            {
                updates.push((
                    candidate.candidate_id.clone(),
                    candidate.epoch_id.clone(),
                    candidate_status,
                    epoch_status,
                ));
            }
        }
        if updates.is_empty() {
            return Ok(());
        }
        let event_updates = updates
            .iter()
            .map(|(candidate_id, _, status, _)| {
                json!({"candidate_id": candidate_id, "status": status})
            })
            .collect::<Vec<_>>();
        self.store.transact(
            "DeploymentStateReconciled",
            None,
            None,
            json!({"updates": event_updates}),
            move |state| {
                for (candidate_id, epoch_id, candidate_status, epoch_status) in &updates {
                    if let Some(candidate) = state
                        .candidates
                        .iter_mut()
                        .find(|candidate| candidate.candidate_id == *candidate_id)
                    {
                        candidate.status = *candidate_status;
                    }
                    if let Some(epoch) = state
                        .epochs
                        .iter_mut()
                        .find(|epoch| epoch.epoch_id == *epoch_id)
                    {
                        epoch.status = *epoch_status;
                        if epoch_status.terminal() {
                            epoch.completed_at.get_or_insert_with(Utc::now);
                        }
                    }
                    if let Some(archive) = state
                        .archive
                        .iter_mut()
                        .find(|archive| archive.candidate_id == *candidate_id)
                    {
                        archive.status = *candidate_status;
                    }
                }
                Ok(())
            },
        )
    }

    pub fn observe_trace(
        &self,
        page: &TaskTracePage,
        independent_group: &str,
    ) -> Result<Option<EvolutionOpportunity>, SupervisorError> {
        self.observe(observation_from_trace(page, independent_group)?)
    }

    /// 只有完整 trace 且跨独立任务重复出现的问题才会进入代码候选 lane。
    fn observe(
        &self,
        bundle: ObservationBundle,
    ) -> Result<Option<EvolutionOpportunity>, SupervisorError> {
        validate_observation(&bundle)?;
        if !bundle.complete || !bundle.unresolved_refs.is_empty() {
            return Err(SupervisorError::GateRejected(
                "incomplete observation cannot enter the evolution lane".to_owned(),
            ));
        }
        let cluster = failure_cluster(&bundle);
        let high_signal = bundle
            .failure_taxonomy
            .iter()
            .any(|value| is_high_signal_failure(value));
        let state = self.store.snapshot()?;
        let existing = state
            .opportunities
            .iter()
            .find(|opportunity| opportunity.failure_cluster == cluster)
            .cloned();
        let mut independent_groups = HashSet::new();
        let mut task_refs = HashSet::new();
        for observation in state
            .observations
            .iter()
            .filter(|observation| failure_cluster(observation) == cluster)
        {
            task_refs.insert(observation.task_id.clone());
            independent_groups.insert(observation.independent_group.clone());
        }
        if let Some(opportunity) = &existing {
            task_refs.extend(opportunity.source_task_refs.iter().cloned());
            independent_groups.extend(opportunity.independent_groups.iter().cloned());
        }
        task_refs.insert(bundle.task_id.clone());
        independent_groups.insert(bundle.independent_group.clone());
        if !high_signal && (task_refs.len() < 2 || independent_groups.len() < 2) {
            let observation = bundle.clone();
            self.store.transact(
                "ObservationRecorded",
                None,
                None,
                json!({
                    "task_id": observation.task_id,
                    "trace_ref": observation.trace_ref,
                    "failure_cluster": cluster,
                }),
                move |state| {
                    if !state
                        .observations
                        .iter()
                        .any(|existing| existing.trace_ref == observation.trace_ref)
                    {
                        state.observations.push(observation);
                    }
                    Ok(())
                },
            )?;
            return Ok(None);
        }
        let mut opportunity = existing.unwrap_or_else(|| EvolutionOpportunity {
            opportunity_id: format!("opportunity-{}", Uuid::now_v7()),
            source_version: bounded(&bundle.source_version, MAX_TEXT),
            source_task_refs: Vec::new(),
            independent_groups: Vec::new(),
            observation_refs: Vec::new(),
            failure_cluster: cluster.clone(),
            suspected_layer: suspected_layer(&bundle),
            causal_hypothesis: format!(
                "repeated {cluster} failures indicate a defect in the suspected execution layer"
            ),
            expected_effect: format!(
                "reduce {cluster} failures without weakening verification, policy, or isolation"
            ),
            confidence: if high_signal { 90 } else { 70 },
            privacy_class: bundle.privacy_class,
            proposed_eval_slices: vec![
                format!("source-task:{}", bundle.task_id),
                "metamorphic-fresh-inputs".to_owned(),
                "security-invariants".to_owned(),
            ],
            created_at: Utc::now(),
        });
        opportunity.source_task_refs.push(bundle.task_id.clone());
        opportunity.source_task_refs.sort();
        opportunity.source_task_refs.dedup();
        opportunity
            .independent_groups
            .push(bundle.independent_group.clone());
        opportunity.independent_groups.sort();
        opportunity.independent_groups.dedup();
        opportunity.observation_refs.extend(
            bundle
                .observation_refs
                .iter()
                .take(MAX_OBSERVATION_REFS)
                .cloned(),
        );
        opportunity.observation_refs.sort();
        opportunity.observation_refs.dedup();
        let stored = self.store.transact(
            "EvolutionOpportunityIdentified",
            None,
            None,
            json!({
                "failure_cluster": cluster,
                "source_task_id": bundle.task_id,
                "trace_ref": bundle.trace_ref,
                "privacy_class": bundle.privacy_class,
            }),
            move |state| {
                if !state
                    .observations
                    .iter()
                    .any(|existing| existing.trace_ref == bundle.trace_ref)
                {
                    state.observations.push(bundle.clone());
                }
                if let Some(existing) = state
                    .opportunities
                    .iter_mut()
                    .find(|existing| existing.failure_cluster == opportunity.failure_cluster)
                {
                    existing
                        .source_task_refs
                        .extend(opportunity.source_task_refs.iter().cloned());
                    existing.source_task_refs.sort();
                    existing.source_task_refs.dedup();
                    existing
                        .independent_groups
                        .extend(opportunity.independent_groups.iter().cloned());
                    existing.independent_groups.sort();
                    existing.independent_groups.dedup();
                    existing
                        .observation_refs
                        .extend(opportunity.observation_refs.iter().cloned());
                    existing.observation_refs.sort();
                    existing.observation_refs.dedup();
                    return Ok(existing.clone());
                }
                state.opportunities.push(opportunity.clone());
                Ok(opportunity)
            },
        )?;
        Ok(Some(stored))
    }

    pub fn start_epoch(
        &self,
        opportunity_id: &str,
        mut budget: EvolutionEpochBudget,
    ) -> Result<EvolutionEpoch, SupervisorError> {
        validate_id(opportunity_id, "opportunity_id")?;
        validate_budget(&budget)?;
        if budget.deadline <= Utc::now() {
            budget.deadline = Utc::now() + chrono::Duration::minutes(1);
        }
        let parent_release_id = self
            .release_store
            .pointer("stable")?
            .map(|pointer| pointer.release_id)
            .ok_or_else(|| {
                SupervisorError::InvalidTransition(
                    "evolution requires a bootstrapped stable release".to_owned(),
                )
            })?;
        let snapshot = self.store.snapshot()?;
        let opportunity = snapshot
            .opportunities
            .iter()
            .find(|opportunity| opportunity.opportunity_id == opportunity_id)
            .ok_or_else(|| SupervisorError::NotFound(format!("opportunity {opportunity_id}")))?;
        if opportunity.source_version != parent_release_id {
            return Err(SupervisorError::GateRejected(format!(
                "opportunity source {} is stale relative to stable release {parent_release_id}",
                opportunity.source_version
            )));
        }
        let epoch = EvolutionEpoch {
            epoch_id: format!("epoch-{}", Uuid::now_v7()),
            opportunity_id: opportunity_id.to_owned(),
            parent_release_id: Some(parent_release_id),
            budget,
            status: EpochStatus::Generating,
            candidate_ids: Vec::new(),
            generation_count: 0,
            holdout_queries: 0,
            created_at: Utc::now(),
            completed_at: None,
            terminal_reason: None,
        };
        let epoch_id = epoch.epoch_id.clone();
        self.store.transact(
            "EvolutionEpochStarted",
            Some(&epoch_id),
            None,
            json!({"opportunity_id": opportunity_id, "budget": epoch.budget}),
            move |state| {
                if !state
                    .opportunities
                    .iter()
                    .any(|opportunity| opportunity.opportunity_id == opportunity_id)
                {
                    return Err(SupervisorError::NotFound(format!(
                        "opportunity {opportunity_id}"
                    )));
                }
                state.epochs.push(epoch.clone());
                Ok(epoch)
            },
        )
    }

    pub fn prepare_candidate_worktree(
        &self,
        epoch_id: &str,
        candidate_id: &str,
    ) -> Result<PathBuf, SupervisorError> {
        validate_id(epoch_id, "epoch_id")?;
        validate_id(candidate_id, "candidate_id")?;
        let snapshot = self.store.snapshot()?;
        let epoch = snapshot
            .epochs
            .iter()
            .find(|epoch| epoch.epoch_id == epoch_id)
            .ok_or_else(|| SupervisorError::NotFound(format!("epoch {epoch_id}")))?;
        if epoch.status != EpochStatus::Generating {
            return Err(SupervisorError::InvalidTransition(format!(
                "epoch {epoch_id} is {:?}, not generating",
                epoch.status
            )));
        }
        let parent_release_id = epoch.parent_release_id.as_deref().ok_or_else(|| {
            SupervisorError::InvalidTransition(
                "candidate generation requires a bootstrapped stable release".to_owned(),
            )
        })?;
        self.ensure_current_stable(parent_release_id)?;
        let parent_source = self.release_store.release_source(parent_release_id)?;
        self.store
            .prepare_candidate_worktree(candidate_id, &parent_source)
    }

    pub fn register_candidate(
        &self,
        proposal: CandidateProposal,
    ) -> Result<EvolutionCandidate, SupervisorError> {
        validate_candidate_proposal(&proposal)?;
        let snapshot = self.store.snapshot()?;
        let parent_release_id = snapshot
            .epochs
            .iter()
            .find(|epoch| epoch.epoch_id == proposal.epoch_id)
            .ok_or_else(|| SupervisorError::NotFound(format!("epoch {}", proposal.epoch_id)))?
            .parent_release_id
            .as_deref()
            .ok_or_else(|| {
                SupervisorError::InvalidTransition(
                    "candidate registration requires a bootstrapped stable release".to_owned(),
                )
            })?;
        self.ensure_current_stable(parent_release_id)?;
        let parent_source = self.release_store.release_source(parent_release_id)?;
        let worktree = validate_candidate_worktree(
            &self.store.paths().root,
            &proposal.worktree,
            &proposal.target_paths,
        )?;
        let actual_target_paths =
            validate_candidate_changes(&parent_source, &worktree, &proposal.target_paths)?;
        let computed_digest = candidate_tree_digest(&worktree)?;
        let confirmed_target_paths =
            validate_candidate_changes(&parent_source, &worktree, &proposal.target_paths)?;
        if confirmed_target_paths != actual_target_paths
            || candidate_tree_digest(&worktree)? != computed_digest
        {
            return Err(SupervisorError::Integrity(
                "candidate source changed while it was being frozen".to_owned(),
            ));
        }
        if computed_digest != proposal.patch_digest {
            return Err(SupervisorError::GateRejected(format!(
                "candidate digest mismatch: declared {}, computed {}",
                proposal.patch_digest, computed_digest
            )));
        }
        let candidate_id = proposal
            .candidate_id
            .clone()
            .unwrap_or_else(|| format!("candidate-{}", Uuid::now_v7()));
        validate_id(&candidate_id, "candidate_id")?;
        let proposal_epoch_id = proposal.epoch_id.clone();
        let event_epoch_id = proposal_epoch_id.clone();
        let candidate_id_for_event = candidate_id.clone();
        let candidate = self.store.transact(
            "CodeCandidateProposed",
            Some(&event_epoch_id),
            Some(&candidate_id_for_event),
            json!({
                "producer_kind": proposal.producer_kind,
                "patch_digest": proposal.patch_digest,
                "target_paths": actual_target_paths,
            }),
            move |state| {
                if state
                    .candidates
                    .iter()
                    .any(|candidate| candidate.candidate_id == candidate_id)
                {
                    return Err(SupervisorError::Invalid(format!(
                        "candidate {candidate_id} already exists"
                    )));
                }
                let epoch = state
                    .epochs
                    .iter_mut()
                    .find(|epoch| epoch.epoch_id == proposal_epoch_id)
                    .ok_or_else(|| {
                        SupervisorError::NotFound(format!("epoch {}", proposal_epoch_id))
                    })?;
                if epoch.status != EpochStatus::Generating {
                    return Err(SupervisorError::Invalid(format!(
                        "epoch {} is {:?}, not generating",
                        epoch.epoch_id, epoch.status
                    )));
                }
                if u32::try_from(epoch.candidate_ids.len()).unwrap_or(u32::MAX)
                    >= epoch.budget.max_candidates
                {
                    return Err(SupervisorError::BudgetExhausted(
                        "epoch candidate budget exhausted".to_owned(),
                    ));
                }
                let opportunity = state
                    .opportunities
                    .iter()
                    .find(|opportunity| opportunity.opportunity_id == epoch.opportunity_id)
                    .ok_or_else(|| {
                        SupervisorError::NotFound(format!("opportunity {}", epoch.opportunity_id))
                    })?;
                let candidate = EvolutionCandidate {
                    candidate_id: candidate_id.clone(),
                    epoch_id: proposal_epoch_id.clone(),
                    opportunity_id: opportunity.opportunity_id.clone(),
                    producer_kind: proposal.producer_kind,
                    producer_version: bounded(&proposal.producer_version, 256),
                    source_commit: bounded(&proposal.source_commit, 512),
                    worktree,
                    patch_digest: proposal.patch_digest.clone(),
                    target_paths: actual_target_paths.clone(),
                    change_class: bounded(&proposal.change_class, 256),
                    generation_model: bounded(&proposal.generation_model, 256),
                    generation_config_digest: bounded(&proposal.generation_config_digest, 256),
                    risk_level: proposal.risk_level,
                    state_migration_ref: proposal.state_migration_ref.clone(),
                    rollback_plan: bounded(&proposal.rollback_plan, MAX_TEXT),
                    status: CandidateStatus::Frozen,
                    release_id: None,
                    trusted_build: false,
                    created_at: Utc::now(),
                    frozen_at: Utc::now(),
                };
                epoch.candidate_ids.push(candidate.candidate_id.clone());
                epoch.generation_count = epoch.generation_count.saturating_add(1);
                epoch.status = EpochStatus::Evaluating;
                state.archive.push(CandidateArchiveEntry {
                    candidate_id: candidate.candidate_id.clone(),
                    lineage_parent_ids: epoch.parent_release_id.iter().cloned().collect(),
                    build_digest: None,
                    capability_slice_scores: Default::default(),
                    novelty_descriptor: candidate.change_class.clone(),
                    descendant_success_rate_milli: None,
                    improvement_cost_milli_usd: None,
                    rollback_rate_milli: None,
                    status: candidate.status,
                });
                state.candidates.push(candidate.clone());
                Ok(candidate)
            },
        )?;
        Ok(candidate)
    }

    /// 统一 internal/external producer 的边界：producer 只能返回候选提案，不能直接触碰
    /// evaluation、release pointer 或部署状态。
    pub async fn produce_and_register<P: CandidateProducer>(
        &self,
        producer: &P,
        mut request: CandidateRequest,
    ) -> Result<EvolutionCandidate, SupervisorError> {
        validate_candidate_request(&request)?;
        request.worktree = validate_producer_worktree(&self.store.paths().root, &request.worktree)?;
        let state = self.store.snapshot()?;
        let epoch = state
            .epochs
            .iter()
            .find(|epoch| epoch.epoch_id == request.epoch_id)
            .ok_or_else(|| SupervisorError::NotFound(format!("epoch {}", request.epoch_id)))?;
        if epoch.status != EpochStatus::Generating {
            return Err(SupervisorError::InvalidTransition(format!(
                "epoch {} is {:?}, not generating",
                epoch.epoch_id, epoch.status
            )));
        }
        let opportunity = state
            .opportunities
            .iter()
            .find(|opportunity| opportunity.opportunity_id == epoch.opportunity_id)
            .ok_or_else(|| {
                SupervisorError::NotFound(format!("opportunity {}", epoch.opportunity_id))
            })?;
        if request.opportunity != *opportunity
            || request.source_version != opportunity.source_version
        {
            return Err(SupervisorError::GateRejected(
                "candidate request does not match the frozen epoch opportunity".to_owned(),
            ));
        }
        let proposal = producer.produce(request).await?;
        self.register_candidate(proposal)
    }

    fn evaluate_input(
        &self,
        input: EvaluationInput,
    ) -> Result<GeneralizationGateResult, SupervisorError> {
        validate_evaluation_input(&input)?;
        let snapshot = self.store.snapshot()?;
        let candidate = snapshot
            .candidates
            .iter()
            .find(|candidate| candidate.candidate_id == input.candidate_id)
            .cloned()
            .ok_or_else(|| {
                SupervisorError::NotFound(format!("candidate {}", input.candidate_id))
            })?;
        let epoch = snapshot
            .epochs
            .iter()
            .find(|epoch| epoch.epoch_id == candidate.epoch_id)
            .cloned()
            .ok_or_else(|| SupervisorError::NotFound(format!("epoch {}", candidate.epoch_id)))?;
        if candidate.status != CandidateStatus::Frozen {
            return Err(SupervisorError::Invalid(format!(
                "candidate {} is {:?}, not frozen",
                candidate.candidate_id, candidate.status
            )));
        }
        if Utc::now() > epoch.budget.deadline {
            return Err(SupervisorError::BudgetExhausted(
                "evolution epoch deadline exceeded".to_owned(),
            ));
        }
        if input.holdout_queries > epoch.budget.max_holdout_queries
            || epoch.holdout_queries.saturating_add(input.holdout_queries)
                > epoch.budget.max_holdout_queries
        {
            return Err(SupervisorError::BudgetExhausted(
                "sealed holdout disclosure budget exhausted".to_owned(),
            ));
        }
        let campaign_id = format!("campaign-{}", Uuid::now_v7());
        let gate = build_gate_result(&input, &campaign_id, &epoch.budget);
        let campaign = EvaluationCampaign {
            campaign_id: campaign_id.clone(),
            candidate_id: input.candidate_id.clone(),
            baseline_release_id: epoch.parent_release_id.clone(),
            evaluator_version: "golutra-supervisor-evaluator-v1".to_owned(),
            dataset_partition_refs: vec![
                "development-regression".to_owned(),
                "sealed-holdout-threshold".to_owned(),
                "fresh-metamorphic".to_owned(),
            ],
            disclosure_budget_ref: format!("budget:{}", epoch.opportunity_id),
            environment_digest: digest_json(&json!({
                "candidate": input.candidate_id,
                "seeds": [0, 1, 2],
                "sandbox": "sealed-v1",
            })),
            seeds: vec![0, 1, 2],
            created_at: Utc::now(),
            completed_at: Some(Utc::now()),
            sealed_verdict: input.sealed_verdict,
            fresh_verdict: input.fresh_verdict,
        };
        let candidate_id = input.candidate_id.clone();
        let event_candidate_id = candidate_id.clone();
        let candidate_epoch_id = candidate.epoch_id.clone();
        let event_epoch_id = candidate_epoch_id.clone();
        self.store.transact(
            "GeneralizationGateCompleted",
            Some(&event_epoch_id),
            Some(&event_candidate_id),
            json!({
                "campaign_id": campaign_id,
                "verdict": gate.verdict,
                "sealed_verdict": gate.sealed_verdict,
                "fresh_verdict": gate.fresh_verdict,
                "rejection_reasons": gate.rejection_reasons,
            }),
            move |state| {
                let epoch = state
                    .epochs
                    .iter_mut()
                    .find(|epoch| epoch.epoch_id == candidate_epoch_id)
                    .ok_or_else(|| {
                        SupervisorError::NotFound(format!("epoch {}", candidate_epoch_id))
                    })?;
                epoch.holdout_queries = epoch.holdout_queries.saturating_add(input.holdout_queries);
                let stored = state
                    .candidates
                    .iter_mut()
                    .find(|stored| stored.candidate_id == candidate_id)
                    .ok_or_else(|| {
                        SupervisorError::NotFound(format!("candidate {candidate_id}"))
                    })?;
                stored.status = match gate.verdict {
                    GateVerdict::Pass => CandidateStatus::Passed,
                    GateVerdict::Fail | GateVerdict::Inconclusive => CandidateStatus::Rejected,
                };
                epoch.status = match gate.verdict {
                    GateVerdict::Pass => EpochStatus::AwaitingPromotion,
                    GateVerdict::Fail => EpochStatus::Rejected,
                    GateVerdict::Inconclusive => EpochStatus::Inconclusive,
                };
                if epoch.status.terminal() {
                    epoch.completed_at = Some(Utc::now());
                    epoch.terminal_reason = gate.rejection_reasons.first().cloned();
                }
                if let Some(archive) = state
                    .archive
                    .iter_mut()
                    .find(|archive| archive.candidate_id == candidate_id)
                {
                    archive.status = stored.status;
                }
                state.campaigns.push(campaign.clone());
                state.gate_results.push(gate.clone());
                let budget_id = format!("budget:{}", epoch.opportunity_id);
                if let Some(budget) = state
                    .disclosure_budgets
                    .iter_mut()
                    .find(|budget| budget.budget_id == budget_id)
                {
                    budget.query_count = budget.query_count.saturating_add(input.holdout_queries);
                    budget.aggregate_feedback_count =
                        budget.aggregate_feedback_count.saturating_add(1);
                    if budget.query_count >= budget.maximum_queries {
                        budget.exhausted_at = Some(Utc::now());
                    }
                } else {
                    state.disclosure_budgets.push(DisclosureBudget {
                        budget_id,
                        candidate_family_id: epoch.opportunity_id.clone(),
                        maximum_queries: epoch.budget.max_holdout_queries,
                        query_count: input.holdout_queries,
                        aggregate_feedback_count: 1,
                        exact_feedback_count: 0,
                        exhausted_at: (input.holdout_queries >= epoch.budget.max_holdout_queries)
                            .then_some(Utc::now()),
                    });
                }
                Ok(gate)
            },
        )
    }

    pub async fn run_trusted_build(
        &self,
        candidate_id: &str,
    ) -> Result<BuildReport, SupervisorError> {
        let candidate = self.candidate(candidate_id)?;
        if candidate.status != CandidateStatus::Passed {
            return Err(SupervisorError::GateRejected(
                "trusted build requires a candidate that passed evaluation".to_owned(),
            ));
        }
        self.validate_frozen_candidate_source(&candidate)?;
        let diagnostic_root = tempfile::Builder::new()
            .prefix("trusted-check-")
            .tempdir_in(&self.store.paths().artifacts_root)
            .map_err(|error| SupervisorError::Io(error.to_string()))?;
        let artifact_root = diagnostic_root.path().join(candidate_id);
        let report = TrustedBuilder::new()
            .run(&candidate.worktree, &artifact_root)
            .await?;
        if report.source_digest != candidate.patch_digest {
            return Err(SupervisorError::Integrity(
                "trusted build source digest does not match the frozen candidate".to_owned(),
            ));
        }
        self.validate_frozen_candidate_source(&candidate)?;
        Ok(report)
    }

    pub async fn bootstrap_stable_release(
        &self,
        source_root: impl Into<PathBuf>,
    ) -> Result<ReleaseManifest, SupervisorError> {
        if self.release_store.pointer("stable")?.is_some() {
            return Err(SupervisorError::InvalidTransition(
                "a stable release is already bootstrapped".to_owned(),
            ));
        }
        let source_root = source_root.into();
        let bootstrap_id = format!("bootstrap-{}", Uuid::now_v7());
        let artifact_root = self.store.paths().artifacts_root.join(&bootstrap_id);
        let report = TrustedBuilder::new()
            .run(&source_root, &artifact_root)
            .await?;
        if !report.passed || !report.sandbox_enforced {
            return Err(SupervisorError::GateRejected(
                "bootstrap requires a passed OS-enforced trusted build".to_owned(),
            ));
        }
        evaluation_runner::evaluation_worker_artifact(&report)?;
        if self.release_store.pointer("stable")?.is_some() {
            return Err(SupervisorError::InvalidTransition(
                "another process bootstrapped the stable release".to_owned(),
            ));
        }
        let dependency_lock_digest = digest_required_file(&source_root.join("Cargo.lock"))?;
        let toolchain_digest = digest_toolchain(&source_root)?;
        let provenance_ref = self
            .store
            .store_artifact("build-report", &serde_json::to_vec(&report)?)?;
        let update_metadata_ref = self.store.store_artifact(
            "update-metadata",
            &serde_json::to_vec(&json!({
                "candidate_id": bootstrap_id,
                "source_digest": report.source_digest,
                "parent_release_id": null,
                "protocol_version_range": runtime_protocol_version_range(),
                "state_schema_version_range": "1..=2",
            }))?,
        )?;
        let manifest = self.release_store.build_checked(
            ReleaseBuildRequest {
                candidate_id: bootstrap_id.clone(),
                parent_release_id: None,
                source_root,
                source_commit: "bootstrap-local-source".to_owned(),
                dependency_lock_digest,
                toolchain_digest,
                protocol_version_range: runtime_protocol_version_range(),
                state_schema_version_range: "1..=2".to_owned(),
                migration_plan_ref: None,
                provenance_ref,
                update_metadata_ref,
                rollback_release_id: None,
            },
            &report,
            &artifact_root,
        )?;
        self.release_store.set_preview(&manifest.release_id)?;
        self.release_store.start_canary(&manifest.release_id)?;
        self.release_store
            .promote(&manifest.release_id, "initial trusted bootstrap")?;
        let event_manifest = manifest.clone();
        self.store.transact(
            "StableReleaseBootstrapped",
            None,
            None,
            json!({
                "release_id": manifest.release_id,
                "source_digest": manifest.source_digest,
            }),
            move |_| Ok(event_manifest),
        )
    }

    pub fn build_verified_release(
        &self,
        candidate_id: &str,
    ) -> Result<ReleaseManifest, SupervisorError> {
        let state = self.store.snapshot()?;
        let build = state
            .evaluation_builds
            .iter()
            .find(|build| build.candidate_id == candidate_id)
            .ok_or_else(|| {
                SupervisorError::GateRejected(
                    "release requires the trusted build used by paired evaluation".to_owned(),
                )
            })?;
        let report_bytes = serde_json::to_vec(&build.report)?;
        self.store
            .verify_artifact("build-report", &build.report_ref, &report_bytes)?;
        evaluation_runner::evaluation_worker_artifact(&build.report)?;
        self.build_release_from_report(candidate_id, &build.report)
    }

    fn build_release_from_report(
        &self,
        candidate_id: &str,
        report: &BuildReport,
    ) -> Result<ReleaseManifest, SupervisorError> {
        validate_id(candidate_id, "candidate_id")?;
        let snapshot = self.store.snapshot()?;
        let candidate = snapshot
            .candidates
            .iter()
            .find(|candidate| candidate.candidate_id == candidate_id)
            .cloned()
            .ok_or_else(|| SupervisorError::NotFound(format!("candidate {candidate_id}")))?;
        if candidate.status != CandidateStatus::Passed {
            return Err(SupervisorError::GateRejected(format!(
                "candidate {candidate_id} has not passed the generalization gate"
            )));
        }
        if matches!(
            candidate.risk_level,
            CandidateRisk::High | CandidateRisk::Critical
        ) {
            return Err(SupervisorError::GateRejected(
                "high-risk runtime code requires human release review".to_owned(),
            ));
        }
        self.validate_frozen_candidate_source(&candidate)?;
        if report.source_digest != candidate.patch_digest {
            return Err(SupervisorError::Integrity(
                "trusted build report does not match the frozen candidate".to_owned(),
            ));
        }
        let parent_release_id = snapshot
            .epochs
            .iter()
            .find(|epoch| epoch.epoch_id == candidate.epoch_id)
            .and_then(|epoch| epoch.parent_release_id.clone());
        let artifact_root = self.store.paths().artifacts_root.join(candidate_id);
        let dependency_lock_digest = digest_required_file(&candidate.worktree.join("Cargo.lock"))?;
        let toolchain_digest = digest_toolchain(&candidate.worktree)?;
        let provenance_ref = self
            .store
            .store_artifact("build-report", &serde_json::to_vec(report)?)?;
        let update_metadata_ref = self.store.store_artifact(
            "update-metadata",
            &serde_json::to_vec(&json!({
                "candidate_id": candidate.candidate_id,
                "source_digest": report.source_digest,
                "parent_release_id": parent_release_id,
                "protocol_version_range": runtime_protocol_version_range(),
                "state_schema_version_range": "1..=2",
            }))?,
        )?;
        let manifest = self.release_store.build_checked(
            ReleaseBuildRequest {
                candidate_id: candidate.candidate_id.clone(),
                parent_release_id: parent_release_id.clone(),
                source_root: candidate.worktree.clone(),
                source_commit: candidate.source_commit.clone(),
                dependency_lock_digest,
                toolchain_digest,
                protocol_version_range: runtime_protocol_version_range(),
                state_schema_version_range: "1..=2".to_owned(),
                migration_plan_ref: candidate.state_migration_ref.clone(),
                provenance_ref,
                update_metadata_ref,
                rollback_release_id: snapshot
                    .epochs
                    .iter()
                    .find(|epoch| epoch.epoch_id == candidate.epoch_id)
                    .and_then(|epoch| epoch.parent_release_id.clone()),
            },
            report,
            &artifact_root,
        )?;
        let release_id = manifest.release_id.clone();
        let candidate_epoch_id = candidate.epoch_id.clone();
        let event_epoch_id = candidate_epoch_id.clone();
        let event_candidate_id = candidate_id.to_owned();
        self.store.transact(
            "ReleaseBuilt",
            Some(&event_epoch_id),
            Some(&event_candidate_id),
            json!({"release_id": release_id, "source_digest": manifest.source_digest}),
            move |state| {
                let stored = state
                    .candidates
                    .iter_mut()
                    .find(|candidate| candidate.candidate_id == candidate_id)
                    .ok_or_else(|| {
                        SupervisorError::NotFound(format!("candidate {candidate_id}"))
                    })?;
                stored.status = CandidateStatus::Built;
                stored.release_id = Some(manifest.release_id.clone());
                stored.trusted_build = true;
                if let Some(archive) = state
                    .archive
                    .iter_mut()
                    .find(|archive| archive.candidate_id == candidate_id)
                {
                    archive.status = stored.status;
                    archive.build_digest = Some(manifest.source_digest.clone());
                }
                if let Some(epoch) = state
                    .epochs
                    .iter_mut()
                    .find(|epoch| epoch.epoch_id == candidate_epoch_id)
                {
                    epoch.status = EpochStatus::BuildingRelease;
                }
                Ok(manifest)
            },
        )
    }

    pub fn preview(&self, candidate_id: &str) -> Result<ReleasePointer, SupervisorError> {
        let candidate = self.candidate(candidate_id)?;
        if candidate.status != CandidateStatus::Built || !candidate.trusted_build {
            return Err(SupervisorError::InvalidTransition(
                "preview requires an OS-enforced trusted build".to_owned(),
            ));
        }
        let release_id = candidate.release_id.clone().ok_or_else(|| {
            SupervisorError::Integrity("built candidate has no release id".to_owned())
        })?;
        let pointer = self.release_store.set_preview(&release_id)?;
        self.update_candidate_deployment(
            candidate_id,
            CandidateStatus::Previewing,
            EpochStatus::Previewing,
            "ReleasePreviewStarted",
            json!({"release_id": release_id}),
        )?;
        Ok(pointer)
    }

    pub fn start_canary(&self, candidate_id: &str) -> Result<ReleasePointer, SupervisorError> {
        let candidate = self.candidate(candidate_id)?;
        if candidate.status != CandidateStatus::Previewing {
            return Err(SupervisorError::InvalidTransition(
                "canary requires a previewing candidate".to_owned(),
            ));
        }
        let release_id = candidate.release_id.clone().ok_or_else(|| {
            SupervisorError::Integrity("previewing candidate has no release id".to_owned())
        })?;
        let pointer = self.release_store.start_canary(&release_id)?;
        self.update_candidate_deployment(
            candidate_id,
            CandidateStatus::Canarying,
            EpochStatus::Canarying,
            "CanaryStarted",
            json!({"release_id": release_id}),
        )?;
        Ok(pointer)
    }

    pub fn record_canary_observation(
        &self,
        observation: DeploymentObservation,
    ) -> Result<bool, SupervisorError> {
        validate_canary_observation(&observation)?;
        let candidate = self.candidate(&observation.candidate_id)?;
        if candidate.status != CandidateStatus::Canarying
            || candidate.release_id.as_deref() != Some(observation.release_id.as_str())
        {
            return Err(SupervisorError::InvalidTransition(
                "observation does not belong to the active canary".to_owned(),
            ));
        }
        let unhealthy = deployment_observation_unhealthy(&observation);
        let candidate_id = observation.candidate_id.clone();
        self.store.transact(
            "CanaryObservationRecorded",
            Some(&candidate.epoch_id),
            Some(&candidate_id),
            json!({
                "release_id": observation.release_id,
                "cohort": observation.cohort,
                "sample_count": observation.sample_count,
                "unhealthy": unhealthy,
            }),
            move |state| {
                state.deployment_observations.push(observation.clone());
                Ok(unhealthy)
            },
        )
    }

    pub fn promote(
        &self,
        candidate_id: &str,
        reason: &str,
    ) -> Result<ReleasePointer, SupervisorError> {
        let candidate = self.candidate(candidate_id)?;
        if candidate.status != CandidateStatus::Canarying {
            return Err(SupervisorError::InvalidTransition(
                "promotion requires a canarying candidate".to_owned(),
            ));
        }
        let release_id = candidate.release_id.clone().ok_or_else(|| {
            SupervisorError::Integrity("canarying candidate has no release id".to_owned())
        })?;
        let canary = self.release_store.pointer("canary")?.ok_or_else(|| {
            SupervisorError::InvalidTransition(
                "promotion requires the candidate to own the active canary pointer".to_owned(),
            )
        })?;
        if canary.release_id != release_id {
            return Err(SupervisorError::InvalidTransition(
                "promotion candidate does not own the active canary pointer".to_owned(),
            ));
        }
        let state = self.store.snapshot()?;
        let observations = state
            .deployment_observations
            .iter()
            .filter(|observation| {
                observation.candidate_id == candidate_id && observation.release_id == release_id
            })
            .collect::<Vec<_>>();
        if observations.is_empty() {
            return Err(SupervisorError::GateRejected(
                "promotion requires a canary health observation for the current release".to_owned(),
            ));
        }
        if observations
            .iter()
            .any(|observation| deployment_observation_unhealthy(observation))
        {
            return Err(SupervisorError::GateRejected(
                "promotion is blocked by an unhealthy canary observation".to_owned(),
            ));
        }
        let pointer = self.release_store.promote(&release_id, reason)?;
        self.update_candidate_deployment(
            candidate_id,
            CandidateStatus::Promoted,
            EpochStatus::Promoted,
            "ReleasePromoted",
            json!({"release_id": release_id, "reason": bounded(reason, 512)}),
        )?;
        Ok(pointer)
    }

    pub fn rollback(
        &self,
        candidate_id: &str,
        reason: &str,
    ) -> Result<ReleasePointer, SupervisorError> {
        let candidate = self.candidate(candidate_id)?;
        let release_id = candidate
            .release_id
            .clone()
            .ok_or_else(|| SupervisorError::Integrity("candidate has no release id".to_owned()))?;
        let pointer = match candidate.status {
            CandidateStatus::Canarying => {
                let canary = self.release_store.pointer("canary")?.ok_or_else(|| {
                    SupervisorError::InvalidTransition(
                        "rollback requires the candidate to own the active canary pointer"
                            .to_owned(),
                    )
                })?;
                if canary.release_id != release_id {
                    return Err(SupervisorError::InvalidTransition(
                        "rollback candidate does not own the active canary pointer".to_owned(),
                    ));
                }
                self.release_store.cancel_canary(&release_id, reason)?
            }
            CandidateStatus::Promoted => {
                if self.release_store.pointer("canary")?.is_some() {
                    return Err(SupervisorError::InvalidTransition(
                        "an active canary must be rolled back before the stable release".to_owned(),
                    ));
                }
                let stable = self.release_store.pointer("stable")?.ok_or_else(|| {
                    SupervisorError::InvalidTransition(
                        "rollback requires the candidate to own the stable pointer".to_owned(),
                    )
                })?;
                if stable.release_id != release_id {
                    return Err(SupervisorError::InvalidTransition(
                        "rollback candidate does not own the stable pointer".to_owned(),
                    ));
                }
                self.release_store.rollback(reason)?
            }
            _ => {
                return Err(SupervisorError::InvalidTransition(
                    "rollback requires the current canary or promoted candidate".to_owned(),
                ));
            }
        };
        self.update_candidate_deployment(
            candidate_id,
            CandidateStatus::RolledBack,
            EpochStatus::RolledBack,
            "ReleaseRolledBack",
            json!({"release_id": release_id, "reason": bounded(reason, 512)}),
        )?;
        Ok(pointer)
    }

    pub fn enforce_deadlines(&self) -> Result<u32, SupervisorError> {
        self.store.transact(
            "EvolutionDeadlinesEnforced",
            None,
            None,
            json!({}),
            |state| {
                let now = Utc::now();
                let mut count = 0_u32;
                for epoch in &mut state.epochs {
                    if !epoch.status.terminal() && epoch.budget.deadline < now {
                        epoch.status = EpochStatus::BudgetExhausted;
                        epoch.completed_at = Some(now);
                        epoch.terminal_reason = Some("epoch deadline exceeded".to_owned());
                        count = count.saturating_add(1);
                    }
                }
                Ok(count)
            },
        )
    }

    pub fn candidate(&self, candidate_id: &str) -> Result<EvolutionCandidate, SupervisorError> {
        let state = self.store.snapshot()?;
        state
            .candidates
            .into_iter()
            .find(|candidate| candidate.candidate_id == candidate_id)
            .ok_or_else(|| SupervisorError::NotFound(format!("candidate {candidate_id}")))
    }

    fn update_candidate_deployment(
        &self,
        candidate_id: &str,
        candidate_status: CandidateStatus,
        epoch_status: EpochStatus,
        event_type: &str,
        payload: serde_json::Value,
    ) -> Result<(), SupervisorError> {
        let candidate_id_owned = candidate_id.to_owned();
        self.store.transact(
            event_type,
            None,
            Some(candidate_id),
            payload,
            move |state| {
                let candidate = state
                    .candidates
                    .iter_mut()
                    .find(|candidate| candidate.candidate_id == candidate_id_owned)
                    .ok_or_else(|| {
                        SupervisorError::NotFound(format!("candidate {candidate_id_owned}"))
                    })?;
                candidate.status = candidate_status;
                let epoch_id = candidate.epoch_id.clone();
                if let Some(epoch) = state
                    .epochs
                    .iter_mut()
                    .find(|epoch| epoch.epoch_id == epoch_id)
                {
                    epoch.status = epoch_status;
                    if epoch_status.terminal() {
                        epoch.completed_at = Some(Utc::now());
                    }
                }
                if let Some(archive) = state
                    .archive
                    .iter_mut()
                    .find(|archive| archive.candidate_id == candidate_id_owned)
                {
                    archive.status = candidate_status;
                }
                Ok(())
            },
        )
    }
}

/// 将完整 TaskTrace 转为 Supervisor 可以消费的脱敏观察窗口。
pub fn observation_from_trace(
    page: &TaskTracePage,
    independent_group: &str,
) -> Result<ObservationBundle, SupervisorError> {
    validate_task_trace_for_ingestion(page)?;
    let mut failure_taxonomy = Vec::new();
    if page
        .verification
        .as_ref()
        .is_some_and(|record| record.result != golutra_core::VerificationResult::Pass)
    {
        failure_taxonomy.push("verification-failure".to_owned());
    }
    for event in &page.events {
        if event_is_failure(event) {
            let summary = event
                .payload
                .get("summary")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("runtime failure")
                .to_ascii_lowercase();
            failure_taxonomy.push(if summary.contains("security") {
                "security-policy-breach".to_owned()
            } else if summary.contains("provider") || summary.contains("auth") {
                "provider-failure".to_owned()
            } else {
                "runtime-failure".to_owned()
            });
        }
    }
    failure_taxonomy.sort();
    failure_taxonomy.dedup();
    let objective = page
        .events
        .iter()
        .find_map(|event| {
            event
                .payload
                .get("prompt")
                .or_else(|| event.payload.get("objective"))
                .and_then(serde_json::Value::as_str)
        })
        .unwrap_or("observed runtime task")
        .chars()
        .take(MAX_TEXT)
        .collect();
    Ok(ObservationBundle {
        task_id: page.task_id.to_string(),
        source_version: page.runtime_identity.clone(),
        trace_ref: format!(
            "runtime://{}/{}?digest={}",
            page.session_id, page.task_id, page.integrity.event_chain_digest
        ),
        complete: page.integrity.complete,
        unresolved_refs: page.integrity.unresolved_refs.clone(),
        failure_taxonomy,
        objective,
        observation_refs: page
            .events
            .iter()
            .map(|event| format!("event:{}", event.id))
            .take(MAX_OBSERVATION_REFS)
            .collect(),
        verification_pass: page
            .verification
            .as_ref()
            .is_some_and(|record| record.result == golutra_core::VerificationResult::Pass),
        privacy_class: PrivacyClass::Redacted,
        independent_group: bounded(independent_group, 256),
    })
}

fn validate_task_trace_for_ingestion(page: &TaskTracePage) -> Result<(), SupervisorError> {
    if page.runtime_identity.trim().is_empty()
        || page.runtime_identity.len() > 256
        || !page.events.iter().any(|event| {
            event
                .payload
                .get("runtime_identity")
                .and_then(serde_json::Value::as_str)
                == Some(page.runtime_identity.as_str())
        })
        || page.events.iter().any(|event| {
            event
                .payload
                .get("runtime_identity")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|identity| identity != page.runtime_identity)
        })
    {
        return Err(SupervisorError::Integrity(
            "task trace runtime identity is absent or inconsistent with its event chain".to_owned(),
        ));
    }
    if page.view != TraceView::Full {
        return Err(SupervisorError::GateRejected(
            "supervisor ingestion requires a redacted full task trace".to_owned(),
        ));
    }
    if !page.integrity.complete
        || page.has_more
        || page.next_cursor.is_some()
        || !page.integrity.unresolved_refs.is_empty()
        || !page.integrity.missing_sections.is_empty()
        || !page.integrity.retention_losses.is_empty()
    {
        return Err(SupervisorError::GateRejected(
            "task trace is incomplete and cannot be ingested".to_owned(),
        ));
    }
    if page.context_snapshots.is_empty()
        || page.verification_plan.is_none()
        || page.verification.is_none()
        || page.post_task_jobs.is_empty()
        || !page.evaluation.terminal
    {
        return Err(SupervisorError::GateRejected(
            "task trace is missing required context, verification, or post-task facts".to_owned(),
        ));
    }
    if page.evaluation.session_id != page.session_id || page.evaluation.task_id != page.task_id {
        return Err(SupervisorError::Integrity(
            "task trace evaluation projection identity does not match the trace".to_owned(),
        ));
    }
    if page
        .context_snapshots
        .iter()
        .any(|snapshot| snapshot.session_id != page.session_id || snapshot.task_id != page.task_id)
        || page
            .verification_plan
            .as_ref()
            .is_none_or(|plan| plan.task_id != page.task_id)
        || page
            .verification
            .as_ref()
            .is_none_or(|record| record.task_id != page.task_id)
        || page
            .post_task_jobs
            .iter()
            .any(|job| job.session_id != page.session_id.to_string() || job.task_id != page.task_id)
    {
        return Err(SupervisorError::Integrity(
            "task trace contains facts from another session or task".to_owned(),
        ));
    }
    let event_count = u64::try_from(page.events.len()).unwrap_or(u64::MAX);
    if event_count == 0 || event_count != page.integrity.event_count {
        return Err(SupervisorError::Integrity(
            "task trace event count does not match its integrity record".to_owned(),
        ));
    }
    for (index, event) in page.events.iter().enumerate() {
        if event.session_id != page.session_id || event.task_id != Some(page.task_id) {
            return Err(SupervisorError::Integrity(
                "task trace event identity does not match the trace".to_owned(),
            ));
        }
        if index > 0 && page.events[index - 1].sequence_no >= event.sequence_no {
            return Err(SupervisorError::Integrity(
                "task trace event sequence is not strictly increasing".to_owned(),
            ));
        }
    }
    let first_sequence = page.events.first().map(|event| event.sequence_no);
    let last_sequence = page.events.last().map(|event| event.sequence_no);
    if page.integrity.first_sequence != first_sequence
        || page.integrity.last_sequence != last_sequence
        || page.integrity.event_chain_digest != task_trace_event_digest(&page.events)?
    {
        return Err(SupervisorError::Integrity(
            "task trace sequence range or event-chain digest is invalid".to_owned(),
        ));
    }
    Ok(())
}

fn task_trace_event_digest(
    events: &[golutra_protocol::RuntimeEvent],
) -> Result<String, SupervisorError> {
    let mut digest = Sha256::new();
    for event in events {
        digest.update(
            i64::try_from(event.sequence_no)
                .unwrap_or(i64::MAX)
                .to_be_bytes(),
        );
        digest.update(serde_json::to_string(event)?.as_bytes());
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
}

fn event_is_failure(event: &golutra_protocol::RuntimeEvent) -> bool {
    match event.event_type {
        golutra_protocol::RuntimeEventType::TaskAborted
        | golutra_protocol::RuntimeEventType::TaskInterrupted
        | golutra_protocol::RuntimeEventType::TaskUncertain
        | golutra_protocol::RuntimeEventType::ProviderAuthFailed => true,
        golutra_protocol::RuntimeEventType::PolicyEvaluated => event
            .payload
            .get("record")
            .and_then(|record| record.get("decision"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|decision| matches!(decision, "deny" | "block")),
        golutra_protocol::RuntimeEventType::TaskCompleted => event
            .payload
            .get("status")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|status| !matches!(status, "completed")),
        _ => false,
    }
}

fn validate_observation(bundle: &ObservationBundle) -> Result<(), SupervisorError> {
    for (name, value) in [
        ("task_id", bundle.task_id.as_str()),
        ("source_version", bundle.source_version.as_str()),
        ("trace_ref", bundle.trace_ref.as_str()),
        ("objective", bundle.objective.as_str()),
        ("independent_group", bundle.independent_group.as_str()),
    ] {
        if value.trim().is_empty() || value.len() > MAX_TEXT {
            return Err(SupervisorError::Invalid(format!(
                "observation {name} is empty or too large"
            )));
        }
    }
    if bundle.privacy_class == PrivacyClass::Restricted {
        return Err(SupervisorError::GateRejected(
            "restricted observations cannot enter the evolution dataset".to_owned(),
        ));
    }
    Ok(())
}

fn validate_budget(budget: &EvolutionEpochBudget) -> Result<(), SupervisorError> {
    if budget.max_candidates == 0
        || budget.max_candidates > 32
        || budget.max_generations == 0
        || budget.max_generations > 64
        || budget.max_holdout_queries > 16
        || budget.max_canary_releases > 4
        || budget.max_latency_delta_ms < 0
        || !budget.max_cost_usd.is_finite()
        || budget.max_cost_usd < 0.0
    {
        return Err(SupervisorError::Invalid(
            "evolution budget is outside the safe bounds".to_owned(),
        ));
    }
    Ok(())
}

fn validate_candidate_proposal(proposal: &CandidateProposal) -> Result<(), SupervisorError> {
    validate_id(&proposal.epoch_id, "epoch_id")?;
    for (name, value) in [
        ("producer_version", proposal.producer_version.as_str()),
        ("source_commit", proposal.source_commit.as_str()),
        ("patch_digest", proposal.patch_digest.as_str()),
        ("change_class", proposal.change_class.as_str()),
        ("generation_model", proposal.generation_model.as_str()),
        (
            "generation_config_digest",
            proposal.generation_config_digest.as_str(),
        ),
        ("rollback_plan", proposal.rollback_plan.as_str()),
    ] {
        if value.trim().is_empty() || value.len() > MAX_TEXT {
            return Err(SupervisorError::Invalid(format!(
                "candidate {name} is empty or too large"
            )));
        }
    }
    if proposal.target_paths.len() > 256 {
        return Err(SupervisorError::Invalid(
            "candidate target path count exceeds the limit".to_owned(),
        ));
    }
    if proposal.rollback_plan.to_ascii_lowercase().contains("none") {
        return Err(SupervisorError::GateRejected(
            "candidate must provide a rollback plan".to_owned(),
        ));
    }
    Ok(())
}

fn validate_candidate_request(request: &CandidateRequest) -> Result<(), SupervisorError> {
    validate_id(&request.epoch_id, "epoch_id")?;
    for (name, value) in [
        ("source_version", request.source_version.as_str()),
        (
            "opportunity_id",
            request.opportunity.opportunity_id.as_str(),
        ),
    ] {
        if value.trim().is_empty() || value.len() > MAX_TEXT {
            return Err(SupervisorError::Invalid(format!(
                "candidate request {name} is empty or too large"
            )));
        }
    }
    if request.observation_bundle_refs.is_empty()
        || request.observation_bundle_refs.len() > MAX_OBSERVATION_REFS
        || request
            .observation_bundle_refs
            .iter()
            .any(|value| value.trim().is_empty() || value.len() > MAX_TEXT)
    {
        return Err(SupervisorError::Invalid(
            "candidate request observation refs are empty or exceed their limits".to_owned(),
        ));
    }
    Ok(())
}

fn validate_evaluation_input(input: &EvaluationInput) -> Result<(), SupervisorError> {
    validate_id(&input.candidate_id, "candidate_id")?;
    if input.paired_execution_refs.len() < 2
        || input
            .paired_execution_refs
            .iter()
            .any(|value| value.trim().is_empty())
    {
        return Err(SupervisorError::GateRejected(
            "evaluation requires distinct baseline and candidate execution refs".to_owned(),
        ));
    }
    let unique = input.paired_execution_refs.iter().collect::<HashSet<_>>();
    if unique.len() < 2 {
        return Err(SupervisorError::GateRejected(
            "baseline and candidate execution refs must differ".to_owned(),
        ));
    }
    if input.paired_execution_refs.iter().any(|value| {
        !value.starts_with("runtime://")
            && !value.starts_with("execution://")
            && !value.starts_with("artifact://regression-trace/")
            && !value.starts_with("artifact://supervisor-evaluation/")
    }) {
        return Err(SupervisorError::GateRejected(
            "paired execution refs must point to durable runtime execution traces".to_owned(),
        ));
    }
    if input.evidence_refs.is_empty() {
        return Err(SupervisorError::GateRejected(
            "evaluation requires durable evidence refs".to_owned(),
        ));
    }
    if !input.quality_delta.is_finite() || !input.cost_delta_usd.is_finite() {
        return Err(SupervisorError::Invalid(
            "evaluation metrics must be finite".to_owned(),
        ));
    }
    Ok(())
}

fn validate_canary_observation(observation: &DeploymentObservation) -> Result<(), SupervisorError> {
    validate_id(&observation.candidate_id, "candidate_id")?;
    validate_id(&observation.release_id, "release_id")?;
    if observation.sample_count == 0 {
        return Err(SupervisorError::Invalid(
            "canary observation sample_count must be positive".to_owned(),
        ));
    }
    Ok(())
}

fn deployment_observation_unhealthy(observation: &DeploymentObservation) -> bool {
    observation.rollback_signal
        || observation.security_violation
        || observation.task_failure_rate_milli > 200
        || observation.cost_delta_milli_usd > 100
        || observation.latency_delta_ms > 5_000
}

fn build_gate_result(
    input: &EvaluationInput,
    campaign_id: &str,
    budget: &EvolutionEpochBudget,
) -> GeneralizationGateResult {
    let mut reasons = Vec::new();
    for (name, verdict) in [
        ("development", input.development_verdict),
        ("security", input.security_verdict),
        ("migration", input.migration_verdict),
        ("sealed", input.sealed_verdict),
        ("fresh", input.fresh_verdict),
    ] {
        if verdict != GateVerdict::Pass {
            reasons.push(format!("{name} gate is {verdict:?}"));
        }
    }
    if input.quality_delta <= 0.0 {
        reasons.push("candidate has no measured quality improvement".to_owned());
    }
    if input.exact_feedback_exposed {
        reasons.push("sealed evaluator exposed exact feedback to candidate".to_owned());
    }
    if input.cost_delta_usd > budget.max_cost_usd {
        reasons.push("candidate cost delta exceeds epoch budget".to_owned());
    }
    if input.latency_delta_ms > budget.max_latency_delta_ms {
        reasons.push("candidate latency delta exceeds epoch budget".to_owned());
    }
    let verdict = if input.development_verdict.eq(&GateVerdict::Fail)
        || input.security_verdict.eq(&GateVerdict::Fail)
        || input.migration_verdict.eq(&GateVerdict::Fail)
        || input.sealed_verdict.eq(&GateVerdict::Fail)
        || input.fresh_verdict.eq(&GateVerdict::Fail)
        || input.quality_delta <= 0.0
        || input.exact_feedback_exposed
        || input.cost_delta_usd > budget.max_cost_usd
        || input.latency_delta_ms > budget.max_latency_delta_ms
    {
        GateVerdict::Fail
    } else if [
        input.development_verdict,
        input.security_verdict,
        input.migration_verdict,
        input.sealed_verdict,
        input.fresh_verdict,
    ]
    .contains(&GateVerdict::Inconclusive)
    {
        GateVerdict::Inconclusive
    } else {
        GateVerdict::Pass
    };
    GeneralizationGateResult {
        campaign_id: campaign_id.to_owned(),
        candidate_id: input.candidate_id.clone(),
        development_verdict: input.development_verdict,
        sealed_verdict: input.sealed_verdict,
        fresh_verdict: input.fresh_verdict,
        security_verdict: input.security_verdict,
        migration_verdict: input.migration_verdict,
        paired_execution_refs: input.paired_execution_refs.clone(),
        quality_delta_milli: (input.quality_delta * 1_000.0).round() as i32,
        cost_delta_milli_usd: (input.cost_delta_usd * 1_000.0).round() as i64,
        latency_delta_ms: input.latency_delta_ms,
        verdict,
        rejection_reasons: reasons,
        created_at: Utc::now(),
    }
}

fn failure_cluster(bundle: &ObservationBundle) -> String {
    let failure = bundle
        .failure_taxonomy
        .first()
        .map(|value| bounded(value, 128).to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "capability-gap".to_owned());
    format!("{}@{}", failure, bounded(&bundle.source_version, 128))
}

fn suspected_layer(bundle: &ObservationBundle) -> String {
    let text =
        format!("{} {}", bundle.failure_taxonomy.join(" "), bundle.objective).to_ascii_lowercase();
    [
        ("context", "context"),
        ("provider", "provider"),
        ("tool", "tools"),
        ("verification", "verification"),
        ("sandbox", "sandbox"),
        ("runtime", "runtime"),
    ]
    .iter()
    .find_map(|(needle, layer)| text.contains(needle).then_some((*layer).to_owned()))
    .unwrap_or_else(|| "runtime-orchestration".to_owned())
}

fn is_high_signal_failure(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    [
        "security",
        "credential",
        "sandbox",
        "data-loss",
        "recovery",
        "integrity",
    ]
    .iter()
    .any(|marker| value.contains(marker))
}

fn validate_id(value: &str, name: &str) -> Result<(), SupervisorError> {
    if value.trim().is_empty()
        || value.len() > 256
        || value.contains('/')
        || value.contains('\\')
        || value.contains("..")
        || value.chars().any(char::is_whitespace)
    {
        return Err(SupervisorError::Invalid(format!(
            "{name} is not a safe identifier"
        )));
    }
    Ok(())
}

fn bounded(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

fn digest_toolchain(source_root: &Path) -> Result<String, SupervisorError> {
    for name in ["rust-toolchain.toml", "rust-toolchain"] {
        let path = source_root.join(name);
        if path.is_file() {
            return digest_required_file(&path);
        }
    }
    Err(SupervisorError::Integrity(
        "trusted source has no rust-toolchain manifest".to_owned(),
    ))
}

fn digest_required_file(path: &Path) -> Result<String, SupervisorError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| SupervisorError::Io(format!("{}: {error}", path.display())))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_PROVENANCE_FILE_BYTES
    {
        return Err(SupervisorError::Integrity(format!(
            "release provenance file violates its boundary: {}",
            path.display()
        )));
    }
    let mut file = fs::File::open(path)
        .map_err(|error| SupervisorError::Io(format!("{}: {error}", path.display())))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| SupervisorError::Io(format!("{}: {error}", path.display())))?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        if total > MAX_PROVENANCE_FILE_BYTES {
            return Err(SupervisorError::Integrity(
                "release provenance file exceeds its size limit".to_owned(),
            ));
        }
        digest.update(&buffer[..read]);
    }
    if total != metadata.len() {
        return Err(SupervisorError::Integrity(
            "release provenance file changed while hashing".to_owned(),
        ));
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
}

fn digest_json(value: &serde_json::Value) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use async_trait::async_trait;
    use golutra_core::{
        BudgetOverflowAction, ContextSnapshot, ContextSnapshotId, PostTaskJob, PostTaskJobId,
        PostTaskJobKind, PostTaskJobStatus, ProviderRequestId, SessionId, TaskClass, TaskId,
        TokenBudgetSnapshot, TokenBudgetSnapshotId, TraceIntegrity, TurnId, VerificationDimensions,
        VerificationId, VerificationPlan, VerificationPlanId, VerificationRecord,
        VerificationResult,
    };
    use golutra_protocol::{
        EvaluationProjection, RuntimeEvent, RuntimeEventSource, RuntimeEventType,
    };
    use tempfile::tempdir;

    use super::*;

    fn setup() -> (EvolutionSupervisor, tempfile::TempDir, tempfile::TempDir) {
        let root = tempdir().expect("supervisor root");
        let releases = tempdir().expect("release root");
        let supervisor =
            EvolutionSupervisor::new(root.path(), releases.path()).expect("supervisor");
        let source = tempdir().expect("stable source");
        fs::create_dir_all(source.path().join("crates/golutra-runtime/src"))
            .expect("stable source directory");
        fs::write(
            source.path().join("crates/golutra-runtime/src/lib.rs"),
            "stable runtime",
        )
        .expect("stable runtime source");
        fs::write(source.path().join("Cargo.lock"), "fixture lock").expect("stable lock");
        fs::write(
            source.path().join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"stable\"\n",
        )
        .expect("stable toolchain");
        let manifest = supervisor
            .release_store()
            .build(ReleaseBuildRequest {
                candidate_id: "bootstrap-test".to_owned(),
                parent_release_id: None,
                source_root: source.path().to_owned(),
                source_commit: "bootstrap-test".to_owned(),
                dependency_lock_digest: "sha256:fixture-lock".to_owned(),
                toolchain_digest: "sha256:fixture-toolchain".to_owned(),
                protocol_version_range: runtime_protocol_version_range(),
                state_schema_version_range: "1..=2".to_owned(),
                migration_plan_ref: None,
                provenance_ref: "supervisor://provenance/bootstrap-test".to_owned(),
                update_metadata_ref: "supervisor://metadata/bootstrap-test".to_owned(),
                rollback_release_id: None,
            })
            .expect("stable release");
        supervisor
            .release_store()
            .set_preview(&manifest.release_id)
            .expect("stable preview");
        supervisor
            .release_store()
            .start_canary(&manifest.release_id)
            .expect("stable canary");
        supervisor
            .release_store()
            .promote(&manifest.release_id, "test bootstrap")
            .expect("stable promote");
        (supervisor, root, releases)
    }

    fn candidate_worktree(
        supervisor: &EvolutionSupervisor,
        epoch_id: &str,
        candidate_id: &str,
        runtime_source: &str,
    ) -> PathBuf {
        let worktree = supervisor
            .prepare_candidate_worktree(epoch_id, candidate_id)
            .expect("prepared candidate worktree");
        fs::write(
            worktree.join("crates/golutra-runtime/src/lib.rs"),
            runtime_source,
        )
        .expect("candidate runtime source");
        worktree
    }

    fn observation(
        supervisor: &EvolutionSupervisor,
        task_id: &str,
        group: &str,
    ) -> ObservationBundle {
        ObservationBundle {
            task_id: task_id.to_owned(),
            source_version: supervisor
                .release_store()
                .pointer("stable")
                .expect("stable pointer")
                .expect("stable release")
                .release_id,
            trace_ref: format!("runtime://{task_id}"),
            complete: true,
            unresolved_refs: Vec::new(),
            failure_taxonomy: vec!["tool-output-contamination".to_owned()],
            objective: "fix tool output context handling".to_owned(),
            observation_refs: vec![format!("event:{task_id}")],
            verification_pass: false,
            privacy_class: PrivacyClass::Redacted,
            independent_group: group.to_owned(),
        }
    }

    fn complete_trace_page() -> TaskTracePage {
        let session_id = SessionId::new();
        let task_id = TaskId::new();
        let turn_id = TurnId::new();
        let event = RuntimeEvent {
            schema_version: golutra_core::RUNTIME_EVENT_SCHEMA_VERSION,
            causal_context: Default::default(),
            causal_links: Vec::new(),
            id: golutra_core::EventId::new(),
            sequence_no: 1,
            session_id,
            turn_id: Some(turn_id),
            task_id: Some(task_id),
            parent_event_id: None,
            event_type: RuntimeEventType::TaskCompleted,
            timestamp: Utc::now(),
            source: RuntimeEventSource::Runtime,
            payload: json!({
                "status": "failed",
                "summary": "provider failure",
                "runtime_identity": "release-test",
            }),
            payload_ref: None,
            durable: true,
        };
        let events = vec![event];
        TaskTracePage {
            session_id,
            task_id,
            runtime_identity: "release-test".to_owned(),
            run_provenance: None,
            view: TraceView::Full,
            events: events.clone(),
            context_snapshots: vec![ContextSnapshot {
                snapshot_id: ContextSnapshotId::new(),
                session_id,
                task_id,
                turn_id,
                provider_request_id: ProviderRequestId::new(),
                provider_id: "mock".to_owned(),
                model_id: "mock".to_owned(),
                contributor_manifest: Vec::new(),
                message_manifest: Vec::new(),
                tool_schema_digests: Vec::new(),
                generation_config_digest: None,
                budget_snapshot: TokenBudgetSnapshot {
                    snapshot_id: TokenBudgetSnapshotId::new(),
                    task_id,
                    turn_id,
                    context_window: 4096,
                    max_output: 256,
                    reserved_output_tokens: 256,
                    planned_input_tokens: 32,
                    planned_tool_tokens: 0,
                    planned_summary_tokens: 0,
                    budget_limit: 3840,
                    budget_policy: "test".to_owned(),
                    action_if_exceeded: BudgetOverflowAction::Block,
                },
                canonical_request_digest: "sha256:request".to_owned(),
                redacted_request_artifact_ref: None,
                restricted_request_artifact_ref: None,
                estimate_source: "test".to_owned(),
                created_at: Utc::now(),
            }],
            artifacts: Vec::new(),
            evidence: Vec::new(),
            verification_plan: Some(VerificationPlan {
                plan_id: VerificationPlanId::new(),
                task_id,
                task_class: TaskClass::PlainConversation,
                criteria: vec!["assistant response".to_owned()],
                assertions: Vec::new(),
                policy_assertions: Vec::new(),
                required_artifact_types: Vec::new(),
                generated_by: "test".to_owned(),
                verifier_versions: vec!["test".to_owned()],
                dimensions: VerificationDimensions::default(),
                revision: 1,
                created_at: Utc::now(),
            }),
            verification: Some(VerificationRecord {
                verification_id: VerificationId::new(),
                task_id,
                objective: "provider failure".to_owned(),
                completion_criteria: vec!["assistant response".to_owned()],
                checks: Vec::new(),
                evidence_refs: Vec::new(),
                result: VerificationResult::Fail,
                policy_status: "allowed".to_owned(),
                residual_risks: vec!["provider failed".to_owned()],
            }),
            post_task_jobs: vec![PostTaskJob {
                job_id: PostTaskJobId::new(),
                kind: PostTaskJobKind::DeepEvaluation,
                workspace_id: "workspace-test".to_owned(),
                session_id: session_id.to_string(),
                task_id,
                input_refs: Vec::new(),
                status: PostTaskJobStatus::Succeeded,
                attempt: 1,
                max_attempts: 3,
                lease_owner: None,
                lease_expires_at: None,
                result_refs: Vec::new(),
                last_error: None,
                created_at: Utc::now(),
                started_at: Some(Utc::now()),
                completed_at: Some(Utc::now()),
            }],
            evaluation: EvaluationProjection {
                session_id,
                task_id,
                reviews: Vec::new(),
                results: Vec::new(),
                improvement_candidates: Vec::new(),
                automation_candidates: Vec::new(),
                regressions: Vec::new(),
                promotion_decisions: Vec::new(),
                failure_diagnoses: Vec::new(),
                diagnostic_slices: Vec::new(),
                replay_capsules: Vec::new(),
                replay_executions: Vec::new(),
                external_evaluations: Vec::new(),
                causal_comparisons: Vec::new(),
                post_task_jobs: Vec::new(),
                terminal: true,
                integrity_warnings: Vec::new(),
            },
            integrity: TraceIntegrity {
                event_count: 1,
                first_sequence: Some(1),
                last_sequence: Some(1),
                event_chain_digest: task_trace_event_digest(&events).expect("digest"),
                unresolved_refs: Vec::new(),
                missing_sections: Vec::new(),
                retention_losses: Vec::new(),
                redacted_fields: vec!["provider_credentials".to_owned()],
                missing_causal_links: Vec::new(),
                orphan_events: Vec::new(),
                broken_lifecycle_pairs: Vec::new(),
                provenance_mismatches: Vec::new(),
                artifact_checksum_failures: Vec::new(),
                external_overlay_failures: Vec::new(),
                complete: true,
            },
            next_cursor: None,
            has_more: false,
        }
    }

    #[test]
    fn trace_ingestion_recomputes_integrity_and_rejects_tampering() {
        let trace = complete_trace_page();
        observation_from_trace(&trace, "group-a").expect("valid trace");

        let mut tampered = trace.clone();
        tampered.events[0].payload = json!({"status": "completed"});
        assert!(matches!(
            observation_from_trace(&tampered, "group-a"),
            Err(SupervisorError::Integrity(_))
        ));

        let mut wrong_runtime = trace.clone();
        wrong_runtime.runtime_identity = "release-other".to_owned();
        assert!(matches!(
            observation_from_trace(&wrong_runtime, "group-a"),
            Err(SupervisorError::Integrity(_))
        ));

        let mut summary = trace;
        summary.view = TraceView::Summary;
        assert!(matches!(
            observation_from_trace(&summary, "group-a"),
            Err(SupervisorError::GateRejected(_))
        ));
    }

    fn proposal(
        _supervisor: &EvolutionSupervisor,
        epoch_id: &str,
        worktree: &std::path::Path,
    ) -> CandidateProposal {
        if !worktree.join("Cargo.lock").exists() {
            fs::write(worktree.join("Cargo.lock"), "fixture lock").expect("fixture lock");
        }
        if !worktree.join("rust-toolchain.toml").exists() {
            fs::write(
                worktree.join("rust-toolchain.toml"),
                "[toolchain]\nchannel = \"stable\"\n",
            )
            .expect("fixture toolchain");
        }
        let digest = candidate_tree_digest(worktree).expect("digest");
        CandidateProposal {
            candidate_id: Some("candidate-1".to_owned()),
            epoch_id: epoch_id.to_owned(),
            producer_kind: ProducerKind::Internal,
            producer_version: "internal-v1".to_owned(),
            source_commit: "commit-v1".to_owned(),
            worktree: worktree.to_owned(),
            patch_digest: digest,
            target_paths: vec!["crates/golutra-runtime/src/lib.rs".to_owned()],
            change_class: "context-budget".to_owned(),
            generation_model: "mock".to_owned(),
            generation_config_digest: "sha256:config".to_owned(),
            risk_level: CandidateRisk::Low,
            state_migration_ref: None,
            rollback_plan: "restore parent release pointer".to_owned(),
        }
    }

    struct NeverProducer;

    #[async_trait]
    impl CandidateProducer for NeverProducer {
        async fn produce(
            &self,
            _request: CandidateRequest,
        ) -> Result<CandidateProposal, SupervisorError> {
            panic!("producer must not start before request validation")
        }
    }

    #[tokio::test]
    async fn producer_request_is_validated_before_untrusted_code_starts() {
        let (supervisor, _root, _releases) = setup();
        assert!(
            supervisor
                .observe(observation(&supervisor, "task-1", "run-a"))
                .expect("first observation")
                .is_none()
        );
        let opportunity = supervisor
            .observe(observation(&supervisor, "task-2", "run-b"))
            .expect("second observation")
            .expect("opportunity");
        let epoch = supervisor
            .start_epoch(&opportunity.opportunity_id, EvolutionEpochBudget::default())
            .expect("epoch");
        let outside = tempdir().expect("outside worktree");
        let error = supervisor
            .produce_and_register(
                &NeverProducer,
                CandidateRequest {
                    epoch_id: epoch.epoch_id,
                    source_version: opportunity.source_version.clone(),
                    opportunity,
                    worktree: outside.path().to_path_buf(),
                    observation_bundle_refs: vec!["observation://task-1".to_owned()],
                },
            )
            .await
            .expect_err("outside worktree must fail before producer starts");

        assert!(
            error
                .to_string()
                .contains("must be inside the supervisor worktrees root")
        );
    }

    fn trusted_report(
        supervisor: &EvolutionSupervisor,
        candidate: &EvolutionCandidate,
    ) -> golutra_release::BuildReport {
        let artifact_root = supervisor
            .store()
            .paths()
            .artifacts_root
            .join(&candidate.candidate_id)
            .join("target/release");
        fs::create_dir_all(&artifact_root).expect("artifact directory");
        let binary_artifacts = [
            ("golutra-cli", b"fixture cli".as_slice()),
            ("golutra-eval-worker", b"fixture eval worker".as_slice()),
        ]
        .into_iter()
        .map(|(name, bytes)| {
            fs::write(artifact_root.join(name), bytes).expect("fixture binary");
            golutra_release::BuildArtifact {
                relative_path: format!("target/release/{name}"),
                checksum: format!("sha256:{:x}", Sha256::digest(bytes)),
                size_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            }
        })
        .collect();
        golutra_release::BuildReport {
            builder_version: "test-builder".to_owned(),
            source_digest: candidate.patch_digest.clone(),
            sandbox_backend: "test-os-sandbox".to_owned(),
            sandbox_enforced: true,
            checks: vec![golutra_release::BuildCheck {
                name: "workspace-gates".to_owned(),
                command: vec!["cargo test --workspace".to_owned()],
                status: golutra_release::BuildStatus::Pass,
                exit_code: Some(0),
                duration_ms: 1,
                output_digest: "sha256:test".to_owned(),
            }],
            binary_artifacts,
            passed: true,
            completed_at: Utc::now(),
        }
    }

    fn record_trusted_evaluation_build(
        supervisor: &EvolutionSupervisor,
        candidate: &EvolutionCandidate,
        report: golutra_release::BuildReport,
    ) {
        let candidate_id = candidate.candidate_id.clone();
        let report_ref = supervisor
            .store()
            .store_artifact(
                "build-report",
                &serde_json::to_vec(&report).expect("report bytes"),
            )
            .expect("report artifact");
        supervisor
            .store()
            .transact(
                "TestEvaluationBuildRecorded",
                Some(&candidate.epoch_id),
                Some(&candidate.candidate_id),
                json!({"source_digest": report.source_digest}),
                move |state| {
                    state.evaluation_builds.push(TrustedEvaluationBuild {
                        candidate_id,
                        report_ref,
                        report,
                        completed_at: Utc::now(),
                    });
                    Ok(())
                },
            )
            .expect("evaluation build");
    }

    #[test]
    fn candidate_and_release_digests_share_deterministic_file_order() {
        let source = tempdir().expect("candidate source");
        fs::create_dir_all(source.path().join("crates/golutra-runtime/src"))
            .expect("nested source");
        fs::write(source.path().join("z-last.txt"), "last").expect("last file");
        let script = source.path().join("verify.sh");
        fs::write(&script, "#!/bin/sh\nexit 0\n").expect("script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).expect("script mode");
        }
        fs::write(
            source.path().join("crates/golutra-runtime/src/lib.rs"),
            "runtime",
        )
        .expect("runtime source");
        fs::write(source.path().join("a-first.txt"), "first").expect("first file");

        let candidate_digest = candidate_tree_digest(source.path()).expect("candidate digest");
        let releases = tempdir().expect("release root");
        let release_store = ReleaseStore::new(releases.path()).expect("release store");
        let manifest = release_store
            .build(ReleaseBuildRequest {
                candidate_id: "candidate-digest".to_owned(),
                parent_release_id: None,
                source_root: source.path().to_owned(),
                source_commit: "commit-digest".to_owned(),
                dependency_lock_digest: "sha256:lock".to_owned(),
                toolchain_digest: "sha256:toolchain".to_owned(),
                protocol_version_range: runtime_protocol_version_range(),
                state_schema_version_range: "1..=2".to_owned(),
                migration_plan_ref: None,
                provenance_ref: "supervisor://provenance/candidate-digest".to_owned(),
                update_metadata_ref: "supervisor://metadata/candidate-digest".to_owned(),
                rollback_release_id: None,
            })
            .expect("release manifest");

        assert_eq!(candidate_digest, manifest.source_digest);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let copied = release_store
                .release_source(&manifest.release_id)
                .expect("release source")
                .join("verify.sh");
            assert_ne!(
                fs::metadata(copied)
                    .expect("copied script")
                    .permissions()
                    .mode()
                    & 0o100,
                0
            );
        }
    }

    #[test]
    fn repeated_observations_create_bounded_epoch_and_reject_overfit_gate() {
        let (supervisor, _root, _releases) = setup();
        assert!(
            supervisor
                .observe(observation(&supervisor, "task-1", "run-a"))
                .expect("observe")
                .is_none()
        );
        let opportunity = supervisor
            .observe(observation(&supervisor, "task-2", "run-b"))
            .expect("opportunity")
            .expect("created");
        let epoch = supervisor
            .start_epoch(&opportunity.opportunity_id, EvolutionEpochBudget::default())
            .expect("epoch");
        let worktree = candidate_worktree(&supervisor, &epoch.epoch_id, "candidate-1", "candidate");
        let candidate = supervisor
            .register_candidate(proposal(&supervisor, &epoch.epoch_id, &worktree))
            .expect("candidate");
        let gate = supervisor
            .evaluate_input(EvaluationInput {
                candidate_id: candidate.candidate_id,
                paired_execution_refs: vec![
                    "artifact://regression-trace/baseline-1?checksum=sha256:base".to_owned(),
                    "artifact://regression-trace/candidate-1?checksum=sha256:candidate".to_owned(),
                ],
                development_verdict: GateVerdict::Pass,
                security_verdict: GateVerdict::Pass,
                migration_verdict: GateVerdict::Pass,
                sealed_verdict: GateVerdict::Pass,
                fresh_verdict: GateVerdict::Pass,
                quality_delta: 0.0,
                cost_delta_usd: 0.0,
                latency_delta_ms: 0,
                holdout_queries: 1,
                exact_feedback_exposed: false,
                evidence_refs: vec!["evidence://1".to_owned()],
            })
            .expect("gate");
        assert_eq!(gate.verdict, GateVerdict::Fail);
        assert_eq!(
            supervisor.store().verify_control_log().expect("log").len(),
            5
        );
    }

    #[test]
    fn candidate_control_plane_and_exact_holdout_feedback_are_blocked() {
        let (supervisor, _root, _releases) = setup();
        let mut security_observation = observation(&supervisor, "task-1", "security-a");
        security_observation.failure_taxonomy = vec!["security-policy-breach".to_owned()];
        let opportunity = supervisor
            .observe(security_observation)
            .expect("observe")
            .expect("security opportunity");
        let epoch = supervisor
            .start_epoch(&opportunity.opportunity_id, EvolutionEpochBudget::default())
            .expect("epoch");
        let worktree = candidate_worktree(&supervisor, &epoch.epoch_id, "candidate-2", "candidate");
        let mut blocked = proposal(&supervisor, &epoch.epoch_id, &worktree);
        blocked.target_paths = vec!["crates/golutra-eval/src/lib.rs".to_owned()];
        assert!(supervisor.register_candidate(blocked).is_err());

        let disguised = candidate_worktree(
            &supervisor,
            &epoch.epoch_id,
            "candidate-disguised",
            "allowed runtime change",
        );
        fs::create_dir_all(disguised.join("crates/golutra-supervisor/src"))
            .expect("sealed source directory");
        fs::write(
            disguised.join("crates/golutra-supervisor/src/lib.rs"),
            "hidden control-plane change",
        )
        .expect("sealed source change");
        let error = supervisor
            .register_candidate(CandidateProposal {
                candidate_id: Some("candidate-disguised".to_owned()),
                ..proposal(&supervisor, &epoch.epoch_id, &disguised)
            })
            .expect_err("actual sealed changes must not be hidden by declarations");
        assert!(error.to_string().contains("sealed"));
    }

    #[test]
    fn concurrent_control_plane_initialization_is_idempotent() {
        use std::sync::{Arc, Barrier};

        let root = tempfile::tempdir().expect("supervisor root");
        let releases = tempfile::tempdir().expect("release root");
        let root_path = root.path().to_owned();
        let releases_path = releases.path().to_owned();
        let barrier = Arc::new(Barrier::new(8));
        let results = std::thread::scope(|scope| {
            let handles = (0..8)
                .map(|_| {
                    let barrier = Arc::clone(&barrier);
                    let root_path = root_path.clone();
                    let releases_path = releases_path.clone();
                    scope.spawn(move || {
                        barrier.wait();
                        EvolutionSupervisor::new(root_path, releases_path).is_ok()
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("initialization thread"))
                .collect::<Vec<_>>()
        });

        assert!(results.into_iter().all(|result| result));
    }

    #[tokio::test]
    async fn trusted_build_rejects_source_changed_after_candidate_freeze() {
        let (supervisor, _root, _releases) = setup();
        let mut security_observation = observation(&supervisor, "task-1", "security-a");
        security_observation.failure_taxonomy = vec!["security-policy-breach".to_owned()];
        let opportunity = supervisor
            .observe(security_observation)
            .expect("observe")
            .expect("opportunity");
        let epoch = supervisor
            .start_epoch(&opportunity.opportunity_id, EvolutionEpochBudget::default())
            .expect("epoch");
        let worktree = candidate_worktree(
            &supervisor,
            &epoch.epoch_id,
            "candidate-mutated",
            "frozen candidate",
        );
        let source = worktree.join("crates/golutra-runtime/src/lib.rs");
        let candidate = supervisor
            .register_candidate(CandidateProposal {
                candidate_id: Some("candidate-mutated".to_owned()),
                ..proposal(&supervisor, &epoch.epoch_id, &worktree)
            })
            .expect("candidate");
        supervisor
            .evaluate_input(EvaluationInput {
                candidate_id: candidate.candidate_id.clone(),
                paired_execution_refs: vec![
                    "runtime://baseline-mutated".to_owned(),
                    "runtime://candidate-mutated".to_owned(),
                ],
                development_verdict: GateVerdict::Pass,
                security_verdict: GateVerdict::Pass,
                migration_verdict: GateVerdict::Pass,
                sealed_verdict: GateVerdict::Pass,
                fresh_verdict: GateVerdict::Pass,
                quality_delta: 0.1,
                cost_delta_usd: 0.0,
                latency_delta_ms: 0,
                holdout_queries: 1,
                exact_feedback_exposed: false,
                evidence_refs: vec!["evidence://mutated".to_owned()],
            })
            .expect("gate");
        fs::write(&source, "changed after freeze").expect("mutated source");

        let error = supervisor
            .run_trusted_build(&candidate.candidate_id)
            .await
            .expect_err("mutated candidate must be rejected");
        assert!(error.to_string().contains("changed after it was frozen"));
    }

    #[test]
    fn passed_candidate_moves_through_release_canary_and_rollback() {
        let (supervisor, root, releases) = setup();
        let mut security_observation = observation(&supervisor, "task-1", "security-a");
        security_observation.failure_taxonomy = vec!["security-policy-breach".to_owned()];
        let opportunity = supervisor
            .observe(security_observation)
            .expect("observe")
            .expect("opportunity");
        let epoch = supervisor
            .start_epoch(&opportunity.opportunity_id, EvolutionEpochBudget::default())
            .expect("epoch");
        let worktree = candidate_worktree(
            &supervisor,
            &epoch.epoch_id,
            "candidate-success",
            "candidate release",
        );
        let candidate = supervisor
            .register_candidate(CandidateProposal {
                candidate_id: Some("candidate-success".to_owned()),
                ..proposal(&supervisor, &epoch.epoch_id, &worktree)
            })
            .expect("candidate");
        let gate = supervisor
            .evaluate_input(EvaluationInput {
                candidate_id: candidate.candidate_id.clone(),
                paired_execution_refs: vec![
                    "runtime://baseline-success".to_owned(),
                    "runtime://candidate-success".to_owned(),
                ],
                development_verdict: GateVerdict::Pass,
                security_verdict: GateVerdict::Pass,
                migration_verdict: GateVerdict::Pass,
                sealed_verdict: GateVerdict::Pass,
                fresh_verdict: GateVerdict::Pass,
                quality_delta: 0.12,
                cost_delta_usd: 0.0,
                latency_delta_ms: 0,
                holdout_queries: 1,
                exact_feedback_exposed: false,
                evidence_refs: vec!["evidence://success".to_owned()],
            })
            .expect("gate");
        assert_eq!(gate.verdict, GateVerdict::Pass);
        let mut mismatched_report = trusted_report(&supervisor, &candidate);
        mismatched_report.source_digest = "sha256:wrong-source".to_owned();
        assert!(
            supervisor
                .build_release_from_report(&candidate.candidate_id, &mismatched_report)
                .is_err()
        );
        assert!(
            supervisor
                .build_verified_release(&candidate.candidate_id)
                .expect_err("release without evaluated build must fail")
                .to_string()
                .contains("paired evaluation")
        );
        let report = trusted_report(&supervisor, &candidate);
        record_trusted_evaluation_build(&supervisor, &candidate, report);
        supervisor
            .build_verified_release(&candidate.candidate_id)
            .expect("release");
        supervisor
            .preview(&candidate.candidate_id)
            .expect("preview");
        supervisor
            .start_canary(&candidate.candidate_id)
            .expect("canary");
        let drifted_candidate_id = candidate.candidate_id.clone();
        supervisor
            .store()
            .transact(
                "TestDeploymentStateDrift",
                Some(&candidate.epoch_id),
                Some(&candidate.candidate_id),
                json!({}),
                move |state| {
                    let stored = state
                        .candidates
                        .iter_mut()
                        .find(|candidate| candidate.candidate_id == drifted_candidate_id)
                        .expect("candidate");
                    stored.status = CandidateStatus::Previewing;
                    Ok(())
                },
            )
            .expect("simulate interrupted state mirror");
        let supervisor =
            EvolutionSupervisor::new(root.path(), releases.path()).expect("reconciled supervisor");
        assert_eq!(
            supervisor
                .candidate(&candidate.candidate_id)
                .expect("reconciled candidate")
                .status,
            CandidateStatus::Canarying
        );
        let release_id = supervisor
            .candidate(&candidate.candidate_id)
            .expect("candidate state")
            .release_id
            .expect("release id");
        assert!(
            supervisor
                .promote(&candidate.candidate_id, "no health evidence")
                .expect_err("promotion without a current canary observation must fail")
                .to_string()
                .contains("health observation")
        );
        assert!(
            !supervisor
                .record_canary_observation(DeploymentObservation {
                    candidate_id: candidate.candidate_id.clone(),
                    release_id,
                    cohort: "test".to_owned(),
                    sample_count: 20,
                    task_failure_rate_milli: 0,
                    rollback_signal: false,
                    security_violation: false,
                    cost_delta_milli_usd: 0,
                    latency_delta_ms: 0,
                    observed_at: Utc::now(),
                })
                .expect("healthy canary")
        );
        supervisor
            .promote(&candidate.candidate_id, "healthy canary")
            .expect("promote");
        assert_eq!(
            supervisor
                .candidate(&candidate.candidate_id)
                .expect("promoted state")
                .status,
            CandidateStatus::Promoted
        );

        assert!(
            supervisor
                .start_epoch(&opportunity.opportunity_id, EvolutionEpochBudget::default())
                .expect_err("an opportunity from the previous release must be stale")
                .to_string()
                .contains("stale")
        );
        let mut second_observation = observation(&supervisor, "task-2", "security-b");
        second_observation.failure_taxonomy = vec!["security-policy-breach".to_owned()];
        let second_opportunity = supervisor
            .observe(second_observation)
            .expect("second observation")
            .expect("second opportunity");
        let second_epoch = supervisor
            .start_epoch(
                &second_opportunity.opportunity_id,
                EvolutionEpochBudget::default(),
            )
            .expect("second epoch");
        let second_worktree = candidate_worktree(
            &supervisor,
            &second_epoch.epoch_id,
            "candidate-second",
            "candidate second release",
        );
        let second = supervisor
            .register_candidate(CandidateProposal {
                candidate_id: Some("candidate-second".to_owned()),
                ..proposal(&supervisor, &second_epoch.epoch_id, &second_worktree)
            })
            .expect("second candidate");
        supervisor
            .evaluate_input(EvaluationInput {
                candidate_id: second.candidate_id.clone(),
                paired_execution_refs: vec![
                    "runtime://baseline-second".to_owned(),
                    "runtime://candidate-second".to_owned(),
                ],
                development_verdict: GateVerdict::Pass,
                security_verdict: GateVerdict::Pass,
                migration_verdict: GateVerdict::Pass,
                sealed_verdict: GateVerdict::Pass,
                fresh_verdict: GateVerdict::Pass,
                quality_delta: 0.10,
                cost_delta_usd: 0.0,
                latency_delta_ms: 0,
                holdout_queries: 1,
                exact_feedback_exposed: false,
                evidence_refs: vec!["evidence://second".to_owned()],
            })
            .expect("second gate");
        let report = trusted_report(&supervisor, &second);
        record_trusted_evaluation_build(&supervisor, &second, report);
        supervisor
            .build_verified_release(&second.candidate_id)
            .expect("second release");
        supervisor
            .preview(&second.candidate_id)
            .expect("second preview");
        supervisor
            .start_canary(&second.candidate_id)
            .expect("second canary");
        let second_release_id = supervisor
            .candidate(&second.candidate_id)
            .expect("second candidate state")
            .release_id
            .expect("second release id");
        assert!(
            supervisor
                .record_canary_observation(DeploymentObservation {
                    candidate_id: second.candidate_id.clone(),
                    release_id: second_release_id,
                    cohort: "unhealthy-test".to_owned(),
                    sample_count: 20,
                    task_failure_rate_milli: 250,
                    rollback_signal: false,
                    security_violation: false,
                    cost_delta_milli_usd: 0,
                    latency_delta_ms: 0,
                    observed_at: Utc::now(),
                })
                .expect("unhealthy canary")
        );
        assert!(
            supervisor
                .promote(&second.candidate_id, "unhealthy canary")
                .expect_err("unhealthy canary must not promote")
                .to_string()
                .contains("unhealthy canary")
        );
        supervisor
            .rollback(&second.candidate_id, "canary failure")
            .expect("rollback");
        assert_eq!(
            supervisor
                .release_store()
                .pointer("stable")
                .expect("stable pointer")
                .unwrap()
                .release_id,
            supervisor
                .candidate("candidate-success")
                .expect("first candidate")
                .release_id
                .unwrap()
        );
    }
}
