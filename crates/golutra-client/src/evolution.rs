//! 受治理的开放式任务执行与 Skill 生命周期编排。

use std::{fs, path::PathBuf, sync::Arc, time::Duration};

use golutra_core::{Actor, ActorKind, CommandId, SessionId, TaskStatus};
use golutra_evolution::{
    EnvironmentRecipe, EvolutionPlanner, GeneratedTaskExecution, OpenEndedBudget,
    OpenEndedRunStatus, SkillManifest,
};
use golutra_protocol::{
    CommandAck, RuntimeEventSource, RuntimeEventType, SessionCommand, SessionCommandKind,
};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{
    ClientError, RuntimeExecutionOptions, RuntimeHost, RuntimeHostStorage, RuntimePaths,
    RuntimeStore, ensure_private_dir, host_event, run_blocking, set_owner_only_file,
};

const MAX_GENERATED_TASKS: u32 = 100;
const MAX_SELECTED_TASKS: u32 = 20;
const MAX_TOOL_CALLS_PER_TASK: u32 = 64;
const MAX_RUNTIME_MS_PER_TASK: u64 = 10 * 60 * 1_000;
const MIN_RUNTIME_MS_PER_TASK: u64 = 1_000;
const SKILL_CONTEXT_LIMIT: usize = 3;

impl RuntimeHost {
    pub(super) async fn handle_plan_evolution_command(
        &self,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let objective = command
            .payload
            .get("objective")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("expand verified workspace capabilities")
            .to_owned();
        let budget = command
            .payload
            .get("budget")
            .cloned()
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default();
        validate_budget(&budget)?;

        let evaluation_store = self.evaluation_store.clone();
        let evaluation = run_blocking(move || evaluation_store.snapshot()).await??;
        let plan = EvolutionPlanner.plan(&evaluation, &objective, budget);
        let run_id = plan.run.run_id.clone();
        let selected = plan.run.selected_task_ids.len();
        let evolution_store = self.evolution_store.clone();
        let state = run_blocking(move || evolution_store.record_plan(plan)).await??;
        let run = state
            .runs
            .iter()
            .find(|run| run.run_id == run_id)
            .cloned()
            .ok_or_else(|| {
                ClientError::TaskExecution("recorded evolution run is missing".to_owned())
            })?;

        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            None,
            RuntimeEventType::EvolutionPlanned,
            RuntimeEventSource::Evolution,
            json!({
                "summary": format!("evolution run {run_id} planned with {selected} selected tasks"),
                "command_id": command.command_id,
                "run": run,
            }),
        ))
        .await?;
        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some(format!("evolution run {run_id} planned")),
        })
    }

    pub(super) async fn handle_run_evolution_command(
        self: &Arc<Self>,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let snapshot_store = self.evolution_store.clone();
        let snapshot = run_blocking(move || snapshot_store.snapshot()).await??;
        let requested_run_id = command
            .payload
            .get("run_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let run = requested_run_id
            .and_then(|run_id| snapshot.runs.iter().find(|run| run.run_id == run_id))
            .or_else(|| {
                snapshot
                    .runs
                    .iter()
                    .rev()
                    .find(|run| run.status == OpenEndedRunStatus::Planned)
            })
            .cloned()
            .ok_or_else(|| {
                ClientError::TaskExecution(requested_run_id.map_or_else(
                    || "no planned evolution run is available".to_owned(),
                    |run_id| format!("evolution run {run_id} was not found"),
                ))
            })?;
        validate_budget(&run.budget)?;
        if run.selected_task_ids.is_empty() {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some("evolution run has no selected tasks".to_owned()),
            });
        }

        let run_id = run.run_id.clone();
        let start_store = self.evolution_store.clone();
        run_blocking({
            let run_id = run_id.clone();
            move || start_store.start_run(&run_id)
        })
        .await??;

        let mut failed = 0_usize;
        for generated_task_id in &run.selected_task_ids {
            let generated_task = snapshot
                .generated_tasks
                .iter()
                .find(|task| task.id == *generated_task_id)
                .cloned();
            let recipe = snapshot
                .recipes
                .iter()
                .find(|recipe| recipe.generated_task_id == *generated_task_id)
                .cloned();
            let (Some(generated_task), Some(recipe)) = (generated_task, recipe) else {
                failed = failed.saturating_add(1);
                continue;
            };
            let execution = self
                .execute_generated_task(&run_id, &run.budget, generated_task, recipe, session_id)
                .await;
            match execution {
                Ok(execution) => {
                    failed = failed.saturating_add(usize::from(execution.status != "completed"));
                }
                Err(error) => {
                    failed = failed.saturating_add(1);
                    self.record_event(host_event(
                        self.next_sequence_no(),
                        session_id,
                        None,
                        RuntimeEventType::EvolutionTaskCompleted,
                        RuntimeEventSource::Evolution,
                        json!({
                            "summary": format!("generated task {generated_task_id} failed before runtime completion"),
                            "generated_task_id": generated_task_id,
                            "error": error.to_string(),
                        }),
                    ))
                    .await?;
                }
            }
        }

        let blocked_reason = (failed > 0).then(|| {
            format!(
                "{failed} of {} selected generated tasks did not complete",
                run.selected_task_ids.len()
            )
        });
        let finish_store = self.evolution_store.clone();
        let finished = run_blocking({
            let run_id = run_id.clone();
            let blocked_reason = blocked_reason.clone();
            move || finish_store.finish_run(&run_id, blocked_reason)
        })
        .await??;
        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            None,
            RuntimeEventType::EvolutionCompleted,
            RuntimeEventSource::Evolution,
            json!({
                "summary": format!("evolution run {run_id} finished with {:?}", finished.status),
                "command_id": command.command_id,
                "run": finished,
            }),
        ))
        .await?;
        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some(format!("evolution run {run_id} finished")),
        })
    }

    pub(super) async fn handle_stage_skill_command(
        &self,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let candidate_id = required_string(&command.payload, "candidate_id")?;
        let evaluation_store = self.evaluation_store.clone();
        let candidate = run_blocking(move || evaluation_store.snapshot())
            .await??
            .skill_candidates
            .into_iter()
            .find(|candidate| candidate.id == candidate_id)
            .ok_or_else(|| {
                ClientError::TaskExecution(format!("skill candidate {candidate_id} was not found"))
            })?;
        let evolution_store = self.evolution_store.clone();
        let record = run_blocking(move || evolution_store.stage_skill(&candidate)).await??;
        self.record_skill_event(
            session_id,
            command.command_id,
            RuntimeEventType::SkillStaged,
            "staged",
            &record,
        )
        .await?;
        Ok(skill_ack(
            command.command_id,
            &record.manifest.skill_id,
            "staged",
        ))
    }

    pub(super) async fn handle_review_skill_command(
        &self,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let skill_id = required_string(&command.payload, "skill_id")?;
        let reason = required_string(&command.payload, "reason")?;
        let approved = match command.payload.get("decision").and_then(Value::as_str) {
            Some("approve") => true,
            Some("reject") => false,
            _ => {
                return Ok(CommandAck {
                    command_id: command.command_id,
                    accepted: false,
                    reason: Some("skill review decision must be approve or reject".to_owned()),
                });
            }
        };
        let regression_refs = command
            .payload
            .get("regression_refs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        let reviewer = command.actor.id.clone();
        let evolution_store = self.evolution_store.clone();
        let record = run_blocking({
            let skill_id = skill_id.clone();
            move || {
                evolution_store.review_skill(
                    &skill_id,
                    &reviewer,
                    &reason,
                    regression_refs,
                    approved,
                )
            }
        })
        .await??;
        let action = if approved { "approved" } else { "rejected" };
        self.record_skill_event(
            session_id,
            command.command_id,
            RuntimeEventType::SkillReviewed,
            action,
            &record,
        )
        .await?;
        Ok(skill_ack(command.command_id, &skill_id, action))
    }

    pub(super) async fn handle_install_skill_command(
        &self,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let skill_id = required_string(&command.payload, "skill_id")?;
        let evolution_store = self.evolution_store.clone();
        let record = run_blocking({
            let skill_id = skill_id.clone();
            move || evolution_store.install_skill(&skill_id)
        })
        .await??;
        self.record_skill_event(
            session_id,
            command.command_id,
            RuntimeEventType::SkillInstalled,
            "installed",
            &record,
        )
        .await?;
        Ok(skill_ack(command.command_id, &skill_id, "installed"))
    }

    pub(super) async fn handle_rollback_skill_command(
        &self,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let skill_id = required_string(&command.payload, "skill_id")?;
        let reason = required_string(&command.payload, "reason")?;
        let evolution_store = self.evolution_store.clone();
        let record = run_blocking({
            let skill_id = skill_id.clone();
            move || evolution_store.rollback_skill(&skill_id, &reason)
        })
        .await??;
        self.record_skill_event(
            session_id,
            command.command_id,
            RuntimeEventType::SkillRolledBack,
            "rolled back",
            &record,
        )
        .await?;
        Ok(skill_ack(command.command_id, &skill_id, "rolled back"))
    }

    pub(super) async fn active_skill_context(
        &self,
        objective: &str,
    ) -> Result<Option<String>, ClientError> {
        let evolution_store = self.evolution_store.clone();
        let objective = objective.to_owned();
        let manifests = run_blocking(move || {
            evolution_store.active_skill_context(&objective, SKILL_CONTEXT_LIMIT)
        })
        .await??;
        Ok((!manifests.is_empty()).then(|| render_skill_context(&manifests)))
    }

    async fn execute_generated_task(
        self: &Arc<Self>,
        run_id: &str,
        budget: &OpenEndedBudget,
        generated_task: golutra_eval::GeneratedTask,
        recipe: EnvironmentRecipe,
        parent_session_id: SessionId,
    ) -> Result<GeneratedTaskExecution, ClientError> {
        let paths = self.runtime_paths.as_ref().ok_or_else(|| {
            ClientError::TaskExecution("evolution runs require a durable runtime".to_owned())
        })?;
        let run_component = safe_component(run_id)?;
        let task_component = safe_component(&generated_task.id)?;
        let fixture_root = paths
            .evolution_runs_dir
            .join(run_component)
            .join(task_component);
        let fixture_workspace = fixture_root.join("workspace");
        let manifest_path = fixture_root.join("recipe.json");
        let manifest = serde_json::to_vec_pretty(&json!({
            "run_id": run_id,
            "task": generated_task,
            "recipe": recipe,
            "provider": "deterministic-mock",
            "network": "disabled",
        }))?;
        let fixture_workspace_for_write = fixture_workspace.clone();
        run_blocking(move || {
            ensure_private_dir(&fixture_workspace_for_write)?;
            fs::write(fixture_workspace_for_write.join("README.md"), &manifest)
                .map_err(|error| ClientError::Io(error.to_string()))?;
            fs::write(&manifest_path, &manifest)
                .map_err(|error| ClientError::Io(error.to_string()))?;
            set_owner_only_file(&fixture_workspace_for_write.join("README.md"))?;
            set_owner_only_file(&manifest_path)
        })
        .await??;

        let child_session_id = SessionId::new();
        let execution_id = format!("evolution-execution-{}", Uuid::now_v7());
        let mut execution = GeneratedTaskExecution {
            execution_id: execution_id.clone(),
            run_id: run_id.to_owned(),
            generated_task_id: generated_task.id.clone(),
            runtime_session_id: child_session_id,
            runtime_task_id: None,
            sandbox_workspace: fixture_workspace.display().to_string(),
            status: "running".to_owned(),
            verification_ref: None,
            started_at: chrono::Utc::now(),
            completed_at: None,
        };
        let evolution_store = self.evolution_store.clone();
        let started_execution = execution.clone();
        run_blocking(move || evolution_store.record_execution(started_execution)).await??;
        self.record_event(host_event(
            self.next_sequence_no(),
            parent_session_id,
            None,
            RuntimeEventType::EvolutionTaskStarted,
            RuntimeEventSource::Evolution,
            json!({
                "summary": format!("generated task {} started in an isolated fixture", generated_task.id),
                "execution": execution,
            }),
        ))
        .await?;

        let child = self
            .isolated_fixture_host(&fixture_workspace, child_session_id)
            .await?;
        let command_id = CommandId::new();
        let ack = Box::pin(child.clone().handle_command(SessionCommand {
            command_id,
            session_id: Some(child_session_id),
            kind: SessionCommandKind::Prompt,
            idempotency_key: format!("evolution:{run_id}:{}", generated_task.id),
            actor: Actor {
                kind: ActorKind::Runtime,
                id: format!("evolution:{run_id}"),
            },
            payload: json!({"prompt": generated_task.objective}),
            timestamp: chrono::Utc::now(),
        }))
        .await?;
        if !ack.accepted {
            execution.status = "failed".to_owned();
        } else {
            let timeout = Duration::from_millis(budget.max_runtime_ms_per_task);
            if tokio::time::timeout(
                timeout,
                child.wait_for_finishing_task_control(child_session_id),
            )
            .await
            .is_err()
            {
                if let Some(control) = child
                    .task_controls
                    .lock()
                    .await
                    .get(&child_session_id)
                    .cloned()
                {
                    control.execution.cancel();
                }
                let _ = tokio::time::timeout(
                    Duration::from_secs(5),
                    child.wait_for_finishing_task_control(child_session_id),
                )
                .await;
                execution.status = "cancelled".to_owned();
            }
        }
        let state = child
            .repositories
            .projections
            .state(child_session_id, None)
            .await?;
        execution.runtime_task_id = state.active_task_id;
        execution.verification_ref = state
            .last_verification
            .as_ref()
            .map(|verification| verification.verification_id.to_string());
        if execution.status == "running" {
            execution.status = task_status_name(state.task_status).to_owned();
        }
        execution.completed_at = Some(chrono::Utc::now());
        let evolution_store = self.evolution_store.clone();
        let completed_execution = execution.clone();
        run_blocking(move || evolution_store.record_execution(completed_execution)).await??;
        self.record_event(host_event(
            self.next_sequence_no(),
            parent_session_id,
            None,
            RuntimeEventType::EvolutionTaskCompleted,
            RuntimeEventSource::Evolution,
            json!({
                "summary": format!("generated task {} finished with {}", generated_task.id, execution.status),
                "execution": execution,
            }),
        ))
        .await?;
        Ok(execution)
    }

    async fn isolated_fixture_host(
        &self,
        fixture_workspace: &PathBuf,
        session_id: SessionId,
    ) -> Result<Arc<RuntimeHost>, ClientError> {
        let parent_paths = self.runtime_paths.as_ref().ok_or_else(|| {
            ClientError::TaskExecution("isolated runtime paths are unavailable".to_owned())
        })?;
        let paths = RuntimePaths::from_home_and_cwd(&parent_paths.home, fixture_workspace)?;
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
            golutra_core::ThreadId::new(),
            true,
            RuntimeExecutionOptions::isolated(),
        )
        .await
    }

    async fn record_skill_event(
        &self,
        session_id: SessionId,
        command_id: CommandId,
        event_type: RuntimeEventType,
        action: &str,
        record: &golutra_evolution::SkillLifecycleRecord,
    ) -> Result<(), ClientError> {
        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            None,
            event_type,
            RuntimeEventSource::Evolution,
            json!({
                "summary": format!("skill {} {action}", record.manifest.skill_id),
                "command_id": command_id,
                "record": record,
            }),
        ))
        .await
    }
}

fn validate_budget(budget: &OpenEndedBudget) -> Result<(), ClientError> {
    let valid = budget.max_generated_tasks > 0
        && budget.max_generated_tasks <= MAX_GENERATED_TASKS
        && budget.max_selected_tasks > 0
        && budget.max_selected_tasks <= MAX_SELECTED_TASKS
        && budget.max_selected_tasks <= budget.max_generated_tasks
        && budget.max_tool_calls_per_task > 0
        && budget.max_tool_calls_per_task <= MAX_TOOL_CALLS_PER_TASK
        && (MIN_RUNTIME_MS_PER_TASK..=MAX_RUNTIME_MS_PER_TASK)
            .contains(&budget.max_runtime_ms_per_task);
    if valid {
        Ok(())
    } else {
        Err(ClientError::TaskExecution(
            "evolution budget is outside the governed execution limits".to_owned(),
        ))
    }
}

fn required_string(payload: &Value, key: &str) -> Result<String, ClientError> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| ClientError::TaskExecution(format!("{key} is required")))
}

fn safe_component(value: &str) -> Result<&str, ClientError> {
    if value.is_empty()
        || value.len() > 160
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(ClientError::TaskExecution(
            "evolution identifier contains unsafe path characters".to_owned(),
        ));
    }
    Ok(value)
}

fn task_status_name(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Idle => "idle",
        TaskStatus::Running => "running",
        TaskStatus::WaitingApproval => "waiting_approval",
        TaskStatus::WaitingAuthentication => "waiting_authentication",
        TaskStatus::Pausing => "pausing",
        TaskStatus::Paused => "paused",
        TaskStatus::Aborting => "aborting",
        TaskStatus::Completed => "completed",
        TaskStatus::Partial => "partial",
        TaskStatus::Failed => "failed",
        TaskStatus::Blocked => "blocked",
        TaskStatus::Cancelled => "cancelled",
        TaskStatus::Interrupted => "interrupted",
        TaskStatus::Uncertain => "uncertain",
    }
}

fn render_skill_context(skills: &[SkillManifest]) -> String {
    let rendered = skills
        .iter()
        .map(|skill| {
            format!(
                "Skill: {}\nWhen relevant: {}\nSteps:\n{}",
                skill.name,
                skill.description,
                skill
                    .steps
                    .iter()
                    .enumerate()
                    .map(|(index, step)| format!("{}. {step}", index.saturating_add(1)))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    format!(
        "Verified project skills are optional guidance. Apply only when they match the current objective:\n{rendered}"
    )
}

fn skill_ack(command_id: CommandId, skill_id: &str, action: &str) -> CommandAck {
    CommandAck {
        command_id,
        accepted: true,
        reason: Some(format!("skill {skill_id} {action}")),
    }
}
