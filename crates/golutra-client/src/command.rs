//! SessionCommand 校验、幂等日志与用例分派。

use super::*;
use golutra_core::{
    ApprovalRequest, ApprovalScope, QuestionId, UserQuestionAnswer, UserQuestionResolution,
};
use golutra_protocol::pending_user_question;

const EXPLICIT_COMPACTION_TOKEN_BUDGET: u64 = 2_048;

fn queued_turn_id_from_payload(payload: &Value) -> Option<TurnId> {
    payload
        .get("turn_id")
        .and_then(Value::as_str)
        .and_then(|value| value.parse().ok())
}

fn steering_override_reason(payload: &Value) -> Option<&'static str> {
    if payload.get(crate::task_mode::EXECUTION_MODE_KEY).is_some() {
        return Some("steering cannot change execution_mode; only tool_profile may be overridden");
    }
    if payload
        .get("task_contract")
        .is_some_and(|value| !value.is_null())
    {
        return Some("steering cannot change the active task contract");
    }
    if payload
        .get("output_schema")
        .is_some_and(|value| !value.is_null())
    {
        return Some("steering cannot change the active output schema");
    }
    if payload
        .get("completion_criteria")
        .is_some_and(|value| match value {
            Value::Null => false,
            Value::Array(values) => !values.is_empty(),
            Value::String(value) => !value.trim().is_empty(),
            _ => true,
        })
    {
        return Some("steering cannot change the active completion criteria");
    }
    if payload
        .get("external_verifiers")
        .is_some_and(|value| match value {
            Value::Null => false,
            Value::Array(values) => !values.is_empty(),
            _ => true,
        })
    {
        return Some("steering cannot change the active verifier set");
    }
    if payload
        .get("max_elapsed_ms")
        .is_some_and(|value| !value.is_null())
    {
        return Some("steering cannot change the active elapsed-time budget");
    }
    if payload
        .get("defer_external_verification")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Some("steering cannot change active external-verification behavior");
    }
    if payload
        .get(VERIFY_ON_CHANGE_KEY)
        .is_some_and(|value| !value.is_null())
    {
        return Some("steering cannot change active verify-on-change behavior");
    }
    if payload
        .get(delegation_policy::DELEGATION_COST_BUDGET_KEY)
        .is_some_and(|value| !value.is_null())
    {
        return Some("steering cannot change the active delegation cost budget");
    }
    None
}

fn normalize_inherited_steering_payload(
    payload: &mut Value,
    tool_profile: AgentToolProfile,
    has_explicit_tool_profile: bool,
) -> Result<(), ClientError> {
    if let Some(object) = payload.as_object_mut() {
        for key in [
            crate::task_mode::EXECUTION_MODE_KEY,
            crate::task_mode::NORMALIZED_EXECUTION_MODE_KEY,
            "task_contract",
            "completion_criteria",
            "output_schema",
            "external_verifiers",
            "max_elapsed_ms",
            "defer_external_verification",
            "discover_project_verifiers",
            VERIFY_ON_CHANGE_KEY,
            EXTERNAL_VERIFIERS_REQUIRE_OS_SANDBOX_KEY,
            delegation_policy::DELEGATION_COST_BUDGET_KEY,
        ] {
            object.remove(key);
        }
        if !has_explicit_tool_profile {
            object.remove(TOOL_PROFILE_KEY);
        }
    }
    payload["_task_contract_origin"] = Value::String("active_task".to_owned());
    if has_explicit_tool_profile {
        payload[TOOL_PROFILE_KEY] = serde_json::to_value(tool_profile)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThreadMetadataMutation {
    Upsert,
    Delete,
}

impl RuntimeHost {
    pub(super) async fn reconcile_replayed_delegated_prompt(
        &self,
        session_id: SessionId,
    ) -> Result<(), ClientError> {
        if !self.session_is_delegated_child(session_id).await? {
            return Ok(());
        }
        let state = self
            .storage
            .repositories
            .projections
            .state(session_id, None)
            .await?;
        if !state.task_status.is_active() {
            return Ok(());
        }

        let has_task_control = self
            .execution
            .task_controls
            .lock()
            .await
            .contains_key(&session_id);
        if has_task_control {
            return Ok(());
        }

        // A missing local control is only an orphan if this runtime owns the durable session.
        // Another runtime may still be supervising the child, in which case waiting remains
        // the correct behavior and avoids publishing a competing terminal event.
        let session_lease = match self.try_acquire_session_lease(session_id)? {
            SessionLeaseAttempt::Acquired(lease) => lease,
            SessionLeaseAttempt::Busy => return Ok(()),
        };
        let state = self
            .storage
            .repositories
            .projections
            .state(session_id, None)
            .await?;
        let has_task_control = self
            .execution
            .task_controls
            .lock()
            .await
            .contains_key(&session_id);
        if !state.task_status.is_active() || has_task_control {
            drop(session_lease);
            return Ok(());
        }
        let task_id = state.active_task_id.ok_or_else(|| {
            ClientError::TaskExecution(
                "active delegated child has no durable task id for orphan recovery".to_owned(),
            )
        })?;
        self.record_orphaned_task_recovery(session_id, task_id, "delegated_prompt_replay")
            .await?;
        drop(session_lease);
        Ok(())
    }

    async fn delegated_command_is_authorized(
        &self,
        session_id: SessionId,
        command: &SessionCommand,
    ) -> Result<bool, ClientError> {
        let contains_metadata = crate::delegation::contains_delegation_metadata(&command.payload);
        let delegated_child = self.session_is_delegated_child(session_id).await?;
        let admissions = self.execution.delegation_admissions.lock().await;
        let admission = admissions.get(&session_id);
        if !contains_metadata && !delegated_child && admission.is_none() {
            return Ok(true);
        }
        Ok(admission.is_some_and(|admission| admission.authorizes(command)))
    }

    pub async fn handle_command(
        self: Arc<Self>,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let idempotency_key = command.idempotency_key.trim().to_owned();
        if idempotency_key.is_empty() {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some("idempotency_key is required".to_owned()),
            });
        }
        if idempotency_key.chars().count() > MAX_IDEMPOTENCY_KEY_CHARS {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some(format!(
                    "idempotency_key exceeds {MAX_IDEMPOTENCY_KEY_CHARS} characters"
                )),
            });
        }
        if command.actor.id.trim().is_empty()
            || command.actor.id.chars().count() > MAX_ACTOR_ID_CHARS
        {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some(format!(
                    "actor id must contain 1..={MAX_ACTOR_ID_CHARS} characters"
                )),
            });
        }
        let payload_size = serde_json::to_vec(&command.payload)?.len();
        if payload_size > MAX_COMMAND_PAYLOAD_JSON_BYTES {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some(format!(
                    "command payload exceeds {MAX_COMMAND_PAYLOAD_JSON_BYTES} serialized bytes"
                )),
            });
        }
        let scoped_idempotency_key = self.scoped_idempotency_key(&idempotency_key);
        let session_id = command.session_id.unwrap_or(self.default_session_id);
        self.ensure_session_in_workspace(session_id).await?;
        let _command_guard = self.execution.command_mutex.lock().await;
        if self.execution.shutdown.is_cancelled()
            && matches!(
                &command.kind,
                SessionCommandKind::Create | SessionCommandKind::Prompt
            )
        {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some("runtime host is shutting down".to_owned()),
            });
        }
        let _command_lease = self.acquire_command_lease(&scoped_idempotency_key).await?;
        let command_id = command.command_id;
        let provisional_ack = CommandAck {
            command_id,
            accepted: true,
            reason: Some(PROVISIONAL_COMMAND_ACK_REASON.to_owned()),
        };
        let payload_digest = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&command.payload)?)
        );
        match self
            .claim_command_journal(
                &scoped_idempotency_key,
                command_id,
                &provisional_ack,
                host_event(
                    0,
                    session_id,
                    None,
                    RuntimeEventType::CommandReceived,
                    RuntimeEventSource::Runtime,
                    json!({
                        "summary": "runtime command durably received",
                        "command_id": command_id.to_string(),
                        "kind": command.kind,
                        "actor": &command.actor,
                        "payload_sha256": payload_digest,
                    }),
                ),
            )
            .await?
        {
            CommandClaim::Existing(ack) => {
                // A completed delegated prompt can be replayed after the runtime process that
                // accepted it has exited. The journal must remain idempotent, but its old ack
                // does not imply that this host still has a child supervisor. Reconcile that
                // durable child before the delegation waiter starts polling it.
                if ack.accepted
                    && command.kind == SessionCommandKind::Prompt
                    && command
                        .payload
                        .get(crate::delegation::DELEGATED_TASK_MARKER)
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                {
                    self.reconcile_replayed_delegated_prompt(session_id).await?;
                }
                return Ok(ack);
            }
            CommandClaim::Conflict {
                existing_command_id,
            } => {
                return Ok(CommandAck {
                    command_id,
                    accepted: false,
                    reason: Some(format!(
                        "idempotency key is already assigned to command {existing_command_id}"
                    )),
                });
            }
            CommandClaim::Claimed { .. } => {}
        }
        let result: Result<CommandAck, ClientError> = async {
            let ack = match command.kind {
                SessionCommandKind::Create => {
                    if !self
                        .delegated_command_is_authorized(session_id, &command)
                        .await?
                    {
                        return Ok(CommandAck {
                            command_id,
                            accepted: false,
                            reason: Some(
                                "delegated session metadata is only valid for a host-created child admission"
                                    .to_owned(),
                            ),
                        });
                    }
                    let session_lease = match self.try_acquire_session_lease(session_id)? {
                        SessionLeaseAttempt::Acquired(lease) => lease,
                        SessionLeaseAttempt::Busy => {
                            return Ok(CommandAck {
                                command_id,
                                accepted: false,
                                reason: Some(
                                    "session is active in another Golutra runtime process"
                                        .to_owned(),
                                ),
                            });
                        }
                    };
                    self.ensure_session_in_workspace(session_id).await?;
                    self.upsert_current_thread(session_id, &command.payload)
                        .await?;
                    self.record_event(host_event(
                        self.next_sequence_no(),
                        session_id,
                        None,
                        RuntimeEventType::SessionCreated,
                        RuntimeEventSource::Runtime,
                        json!({
                            "summary": "runtime host created session",
                            "command_id": command_id.to_string(),
                        }),
                    ))
                    .await?;
                    drop(session_lease);
                    CommandAck {
                        command_id,
                        accepted: true,
                        reason: Some(format!("session {session_id} is ready")),
                    }
                }
                SessionCommandKind::RenameThread
                | SessionCommandKind::ArchiveThread
                | SessionCommandKind::DeleteThread => {
                    self.handle_thread_metadata_command(session_id, command)
                        .await?
                }
                SessionCommandKind::Prompt => {
                    self.clone().handle_prompt(session_id, command).await?
                }
                SessionCommandKind::UpdateQueuedTurn => {
                    self.handle_update_queued_turn(session_id, command).await?
                }
                SessionCommandKind::CancelQueuedTurn => {
                    self.handle_cancel_queued_turn(session_id, command).await?
                }
                SessionCommandKind::Abort => {
                    self.handle_lane_command(session_id, &command, "abort")
                        .await?
                }
                SessionCommandKind::ReconcileTask => {
                    self.handle_reconcile_task_command(session_id, command)
                        .await?
                }
                SessionCommandKind::Takeover => {
                    self.handle_takeover_command(session_id, &command).await?
                }
                SessionCommandKind::Pause => {
                    self.handle_lane_command(session_id, &command, "pause")
                        .await?
                }
                SessionCommandKind::Resume => {
                    self.handle_lane_command(session_id, &command, "resume")
                        .await?
                }
                SessionCommandKind::Approve => {
                    self.handle_approval_command(session_id, command, ApprovalDecision::Approved)
                        .await?
                }
                SessionCommandKind::Deny => {
                    self.handle_approval_command(session_id, command, ApprovalDecision::Denied)
                        .await?
                }
                SessionCommandKind::AnswerQuestion => {
                    self.handle_answer_question_command(session_id, command)
                        .await?
                }
                SessionCommandKind::Compact => {
                    self.handle_compact_command(session_id, command).await?
                }
                SessionCommandKind::MemoryRollback => {
                    self.handle_memory_rollback_command(session_id, command)
                        .await?
                }
                SessionCommandKind::MemoryFeedback => {
                    self.handle_memory_feedback_command(session_id, command)
                        .await?
                }
                SessionCommandKind::ReviewMemoryCandidate => {
                    self.handle_review_memory_candidate_command(session_id, command)
                        .await?
                }
                SessionCommandKind::ExpireMemory => {
                    self.handle_expire_memory_command(session_id, command)
                        .await?
                }
                SessionCommandKind::RunRegression | SessionCommandKind::RunRegressionCampaign => {
                    self.handle_regression_command(session_id, command).await?
                }
                SessionCommandKind::Replay => {
                    Box::pin(self.handle_replay_command(session_id, command)).await?
                }
                SessionCommandKind::ReviewCandidate => {
                    self.handle_review_candidate_command(session_id, command)
                        .await?
                }
                SessionCommandKind::ApplyCandidate => {
                    self.handle_apply_candidate_command(session_id, command)
                        .await?
                }
                SessionCommandKind::RollbackCandidate => {
                    self.handle_rollback_candidate_command(session_id, command)
                        .await?
                }
                SessionCommandKind::RecordBenchmark => {
                    self.handle_record_benchmark_command(session_id, command)
                        .await?
                }
                SessionCommandKind::IngestExternalEvaluation => {
                    self.handle_external_evaluation_command(session_id, command)
                        .await?
                }
                SessionCommandKind::CompareCounterfactual => {
                    self.handle_compare_counterfactual_command(session_id, command)
                        .await?
                }
                SessionCommandKind::PlanEvolution => {
                    self.handle_plan_evolution_command(session_id, command)
                        .await?
                }
                SessionCommandKind::RunEvolution => {
                    self.handle_run_evolution_command(session_id, command)
                        .await?
                }
                SessionCommandKind::StageSkill => {
                    self.handle_stage_skill_command(session_id, command).await?
                }
                SessionCommandKind::ReviewSkill => {
                    self.handle_review_skill_command(session_id, command)
                        .await?
                }
                SessionCommandKind::InstallSkill => {
                    self.handle_install_skill_command(session_id, command)
                        .await?
                }
                SessionCommandKind::RollbackSkill => {
                    self.handle_rollback_skill_command(session_id, command)
                        .await?
                }
                SessionCommandKind::ProviderConfigured
                | SessionCommandKind::ProviderAuthSubmitted => {
                    self.handle_provider_configured_command(session_id, command)
                        .await?
                }
                SessionCommandKind::ProviderAuthCancelled => {
                    self.handle_provider_auth_cancelled_command(session_id, command)
                        .await?
                }
                SessionCommandKind::RunStorageMaintenance => {
                    self.handle_storage_maintenance_command(session_id, command)
                        .await?
                }
                SessionCommandKind::WaitPostTaskJob => {
                    self.handle_wait_post_task_job_command(session_id, command)
                        .await?
                }
                SessionCommandKind::RetryPostTaskJob => {
                    self.handle_retry_post_task_job_command(session_id, command)
                        .await?
                }
                SessionCommandKind::Verify | SessionCommandKind::Export => {
                    let reason = format!(
                        "runtime command {:?} is not supported; use the typed verification or thread export entrypoint",
                        command.kind
                    );
                    self.record_event(host_event(
                        self.next_sequence_no(),
                        session_id,
                        None,
                        RuntimeEventType::CommandRejected,
                        RuntimeEventSource::Runtime,
                        json!({
                            "summary": reason.clone(),
                            "command_id": command_id.to_string(),
                            "kind": command.kind,
                        }),
                    ))
                    .await?;
                    CommandAck {
                        command_id,
                        accepted: false,
                        reason: Some(reason),
                    }
                }
            };
            Ok(ack)
        }
        .await;
        match result {
            Ok(ack) => {
                self.complete_command_journal(
                    &scoped_idempotency_key,
                    command_id,
                    &ack,
                    host_event(
                        0,
                        session_id,
                        None,
                        RuntimeEventType::CommandCompleted,
                        RuntimeEventSource::Runtime,
                        json!({
                            "summary": if ack.accepted {
                                "runtime command accepted"
                            } else {
                                "runtime command rejected"
                            },
                            "command_id": command_id.to_string(),
                            "accepted": ack.accepted,
                            "reason": ack.reason,
                        }),
                    ),
                )
                .await?;
                Ok(ack)
            }
            Err(error) => {
                let ack = CommandAck {
                    command_id,
                    accepted: false,
                    reason: Some(error.to_string()),
                };
                self.complete_command_journal(
                    &scoped_idempotency_key,
                    command_id,
                    &ack,
                    host_event(
                        0,
                        session_id,
                        None,
                        RuntimeEventType::CommandCompleted,
                        RuntimeEventSource::Runtime,
                        json!({
                            "summary": "runtime command failed",
                            "command_id": command_id.to_string(),
                            "accepted": false,
                            "reason": ack.reason,
                        }),
                    ),
                )
                .await?;
                Err(error)
            }
        }
    }

    async fn handle_prompt(
        self: Arc<Self>,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let task_id = TaskId::new();
        let turn_id = TurnId::new();
        let mut payload = command.payload.clone();
        let prompt = prompt_from_payload(&payload);
        if prompt.trim().is_empty() {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some("prompt cannot be empty".to_owned()),
            });
        }
        let delegated_payload = payload
            .get(crate::delegation::DELEGATED_TASK_MARKER)
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let steer = payload
            .get("steer")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let steering_override_reason = steer.then(|| steering_override_reason(&payload)).flatten();
        let has_explicit_tool_profile = payload
            .get(TOOL_PROFILE_KEY)
            .is_some_and(|value| !value.is_null());
        if !self
            .delegated_command_is_authorized(session_id, &command)
            .await?
        {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some(
                    "delegated task metadata is only valid for a host-created child task or host admission"
                        .to_owned(),
                ),
            });
        }
        let execution_mode = execution_mode_from_payload(&payload)
            .map_err(|error| ClientError::TaskExecution(error.to_owned()))?;
        let tool_profile = tool_profile_from_payload(&payload)
            .map_err(|error| ClientError::TaskExecution(error.to_owned()))?;
        let has_explicit_task_contract = explicit_task_contract(&payload);
        let apply_legacy_adapter = !steer && should_apply_legacy_adapter(&payload, execution_mode);
        let requested_network = match payload.get("allow_network") {
            None => None,
            Some(Value::Bool(allow_network)) => Some(*allow_network),
            Some(_) => {
                return Err(ClientError::TaskExecution(
                    "allow_network must be a boolean".to_owned(),
                ));
            }
        };
        let requested_yolo = match payload.get("yolo") {
            None => None,
            Some(Value::Bool(yolo)) => Some(*yolo),
            Some(_) => {
                return Err(ClientError::TaskExecution(
                    "yolo must be a boolean".to_owned(),
                ));
            }
        };
        let max_elapsed_ms = match payload.get("max_elapsed_ms") {
            None | Some(Value::Null) => None,
            Some(value) if value.as_u64().is_some_and(|value| value > 0) => value.as_u64(),
            Some(_) => {
                return Err(ClientError::TaskExecution(
                    "max_elapsed_ms must be a positive integer".to_owned(),
                ));
            }
        };
        delegation_policy::cost_budget_from_payload(&payload)
            .map_err(|error| ClientError::TaskExecution(error.to_owned()))?;
        let defer_external_verification = match payload.get("defer_external_verification") {
            None => false,
            Some(Value::Bool(deferred)) => *deferred,
            Some(_) => {
                return Err(ClientError::TaskExecution(
                    "defer_external_verification must be a boolean".to_owned(),
                ));
            }
        };
        let verify_on_change_auto = verify_on_change_auto(&payload)
            .map_err(|error| ClientError::TaskExecution(error.to_owned()))?;
        // Yolo is the trusted full-access profile: it also requests network
        // access, while the host still decides whether that capability exists.
        let requested_network = requested_network
            .map_or(requested_yolo.filter(|enabled| *enabled), |requested| {
                Some(requested || requested_yolo.unwrap_or(false))
            });
        let mut task_contract = task_contract_from_payload(&payload)?;
        let mut contract_origin = if steer {
            "active_task"
        } else if has_explicit_task_contract {
            "explicit"
        } else if apply_legacy_adapter {
            LegacyTaskAdapter::new(&payload, &prompt).apply_to(&mut task_contract);
            "legacy_adapter"
        } else {
            "open"
        };
        let mut discovered_project_verifiers = false;
        let discover_project_verifiers_enabled = match payload.get("discover_project_verifiers") {
            None => true,
            Some(Value::Bool(enabled)) => *enabled,
            Some(_) => {
                return Err(ClientError::TaskExecution(
                    "discover_project_verifiers must be a boolean".to_owned(),
                ));
            }
        };
        let requests_workspace_change = !steer
            && (LegacyTaskAdapter::new(&payload, &prompt).requests_workspace_change()
                || task_contract.requires_workspace_evidence());
        if verify_on_change_auto && requests_workspace_change && !has_explicit_task_contract {
            LegacyTaskAdapter::new(&payload, &prompt).apply_to(&mut task_contract);
            contract_origin = "verify_on_change";
        }
        if !steer
            && discover_project_verifiers_enabled
            && !defer_external_verification
            && payload.get("external_verifiers").is_none()
            && (strict_execution_requested(&payload, execution_mode)
                || (verify_on_change_auto && requests_workspace_change))
            && (task_contract.require_objective_validation
                || task_contract.requires_workspace_evidence())
        {
            let workspace_root = self.execution_workspace_root()?;
            let discovered = discover_project_verifiers(&workspace_root)
                .into_iter()
                .map(|verifier| ExternalVerificationSpec {
                    program: verifier.program,
                    args: verifier.args,
                    cwd: verifier.cwd,
                    timeout_ms: verifier.timeout_ms,
                    expected_exit_code: verifier.expected_exit_code,
                    max_output_bytes: verifier.max_output_bytes,
                })
                .collect::<Vec<_>>();
            if !discovered.is_empty() {
                payload["external_verifiers"] = serde_json::to_value(discovered)?;
                discovered_project_verifiers = true;
            }
        }
        if discovered_project_verifiers {
            task_contract = task_contract_from_payload(&payload)?;
            if apply_legacy_adapter || contract_origin == "verify_on_change" {
                LegacyTaskAdapter::new(&payload, &prompt).apply_to(&mut task_contract);
            }
        }
        if apply_legacy_adapter && defer_external_verification {
            task_contract.require_objective_validation = false;
        }
        payload[EXTERNAL_VERIFIERS_REQUIRE_OS_SANDBOX_KEY] =
            Value::Bool(discovered_project_verifiers);
        if payload.get("external_verifiers").is_none() {
            payload["external_verifiers"] = json!([]);
        }
        task_contract
            .validate()
            .map_err(ClientError::TaskExecution)?;
        let external_verifiers = payload
            .get("external_verifiers")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| {
                ClientError::TaskExecution(format!("invalid external verifier contract: {error}"))
            })?
            .unwrap_or_default();
        payload["task_contract"] = serde_json::to_value(&task_contract)?;
        payload["_task_contract_origin"] = Value::String(contract_origin.to_owned());
        payload[VERIFY_ON_CHANGE_KEY] =
            Value::String(if verify_on_change_auto { "auto" } else { "off" }.to_owned());
        write_normalized_execution_mode(&mut payload, execution_mode);
        if !steer || has_explicit_tool_profile {
            payload[TOOL_PROFILE_KEY] = serde_json::to_value(tool_profile)?;
        }
        let busy_decision = {
            let lane_manager = self.execution.lane_manager.lock().await;
            lane_manager
                .lane(session_id)
                .filter(|lane| is_active_status(lane.status))
                .map(|lane| {
                    let task_id = lane.task_id;
                    lane_manager
                        .decide_busy_policy(
                            session_id,
                            command.command_id,
                            &command.actor,
                            BusyPolicy::Append,
                        )
                        .map(|decision| (task_id, decision))
                })
                .transpose()?
        };
        if let Some((active_task_id, decision)) = busy_decision {
            let mut accepted = decision.applied_policy != BusyPolicy::Reject;
            let mut reason = decision.reason.clone();
            let mut retry_as_new_task = false;
            if accepted {
                let control = self
                    .execution
                    .task_controls
                    .lock()
                    .await
                    .get(&session_id)
                    .cloned();
                match control {
                    Some(control) if control.task_id == active_task_id => {
                        if steer {
                            if let Some(error) = steering_override_reason {
                                accepted = false;
                                reason = error.to_owned();
                            } else {
                                normalize_inherited_steering_payload(
                                    &mut payload,
                                    tool_profile,
                                    has_explicit_tool_profile,
                                )?;
                            }
                        }
                        if accepted
                            && requested_yolo.is_some_and(|requested| control.yolo != requested)
                        {
                            accepted = false;
                            reason =
                                "queued prompt cannot change yolo capability while a task is active"
                                    .to_owned();
                        } else if accepted
                            && requested_network
                                .is_some_and(|requested| control.allow_network != requested)
                        {
                            accepted = false;
                            reason =
                                "queued prompt cannot change network capability while a task is active"
                                    .to_owned();
                        } else if accepted {
                            if let Err(error) = control
                                .provider_settings
                                .normalize_queued_payload(&mut payload)
                            {
                                accepted = false;
                                reason = error.to_owned();
                            } else {
                                let allow_network =
                                    requested_network.unwrap_or(control.allow_network);
                                let yolo = requested_yolo.unwrap_or(control.yolo);
                                payload["allow_network"] = Value::Bool(allow_network);
                                payload["yolo"] = Value::Bool(yolo);
                                if !steer {
                                    payload[EXTERNAL_VERIFIERS_REQUIRE_OS_SANDBOX_KEY] =
                                        Value::Bool(
                                            payload
                                                .get(EXTERNAL_VERIFIERS_REQUIRE_OS_SANDBOX_KEY)
                                                .and_then(Value::as_bool)
                                                .unwrap_or(false)
                                                && !yolo,
                                        );
                                }
                                self.upsert_current_thread(session_id, &payload).await?;
                                let configured_turn =
                                    ConfiguredPendingAgentTurn::new(PendingAgentTurn {
                                        command_id: command.command_id,
                                        turn_id,
                                        content: model_prompt_from_payload(&payload),
                                        task_contract: (!steer).then_some(task_contract.clone()),
                                        output_schema: (!steer)
                                            .then(|| payload.get("output_schema").cloned())
                                            .flatten(),
                                        external_verifiers: if steer {
                                            Vec::new()
                                        } else {
                                            external_verifiers
                                        },
                                        max_elapsed_ms: (!steer)
                                            .then_some(max_elapsed_ms)
                                            .flatten(),
                                        defer_external_verification: !steer
                                            && defer_external_verification,
                                        external_verifiers_require_os_sandbox: !steer
                                            && payload
                                                .get(EXTERNAL_VERIFIERS_REQUIRE_OS_SANDBOX_KEY)
                                                .and_then(Value::as_bool)
                                                .unwrap_or(false),
                                        allow_network,
                                        yolo,
                                        steer,
                                    })
                                    .with_execution_options(
                                        PendingTurnExecutionOptions {
                                            execution_mode: (!steer)
                                                .then(|| execution_mode.explicit())
                                                .flatten(),
                                            tool_profile: (!steer || has_explicit_tool_profile)
                                                .then_some(tool_profile),
                                        },
                                    );
                                match control.execution.reserve_configured_turn(configured_turn) {
                                    Ok(reservation) => {
                                        let transition =
                                            self.execution.lane_manager.lock().await.queue_turn(
                                                session_id,
                                                turn_id,
                                                self.next_sequence_no(),
                                            )?;
                                        if let Err(error) = self
                                            .record_event(with_command_payload(
                                                transition.event,
                                                command.command_id,
                                                payload.clone(),
                                            ))
                                            .await
                                        {
                                            let _ = self
                                                .execution
                                                .lane_manager
                                                .lock()
                                                .await
                                                .discard_queued_turn(session_id, turn_id);
                                            return Err(error);
                                        }
                                        reservation.commit();
                                    }
                                    Err(AgentLoopError::PendingTurnQueueClosed) => {
                                        retry_as_new_task = true;
                                    }
                                    Err(AgentLoopError::PendingTurnQueueFull) => {
                                        accepted = false;
                                        reason =
                                            "active task pending turn queue is full".to_owned();
                                    }
                                    Err(error) => {
                                        return Err(ClientError::TaskExecution(error.to_string()));
                                    }
                                }
                            }
                        }
                    }
                    _ => {
                        retry_as_new_task = true;
                    }
                }
            }
            if retry_as_new_task {
                self.wait_for_finishing_task_control(session_id).await?;
            } else {
                self.record_event(host_event(
                    self.next_sequence_no(),
                    session_id,
                    Some(active_task_id),
                    if accepted {
                        RuntimeEventType::BusyPolicyDecided
                    } else {
                        RuntimeEventType::CommandRejected
                    },
                    RuntimeEventSource::Runtime,
                    json!({
                        "summary": reason.clone(),
                        "command_id": command.command_id.to_string(),
                        "decision": decision,
                        "payload": payload,
                    }),
                ))
                .await?;
                return Ok(CommandAck {
                    command_id: command.command_id,
                    accepted,
                    reason: Some(if accepted {
                        "prompt appended to active runtime lane".to_owned()
                    } else {
                        reason
                    }),
                });
            }
        }
        if steer {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some("steering requires an active runtime task".to_owned()),
            });
        }
        let requested_network = requested_network.unwrap_or(false);
        let yolo = requested_yolo.unwrap_or(false);
        payload["allow_network"] = Value::Bool(requested_network);
        payload["yolo"] = Value::Bool(yolo);
        payload[EXTERNAL_VERIFIERS_REQUIRE_OS_SANDBOX_KEY] = Value::Bool(
            payload
                .get(EXTERNAL_VERIFIERS_REQUIRE_OS_SANDBOX_KEY)
                .and_then(Value::as_bool)
                .unwrap_or(false)
                && !yolo,
        );
        self.wait_for_finishing_task_control(session_id).await?;
        let session_lease = match self.try_acquire_session_lease(session_id)? {
            SessionLeaseAttempt::Acquired(lease) => lease,
            SessionLeaseAttempt::Busy => {
                return Ok(CommandAck {
                    command_id: command.command_id,
                    accepted: false,
                    reason: Some("session is active in another Golutra runtime process".to_owned()),
                });
            }
        };
        let persisted_state = self
            .storage
            .repositories
            .projections
            .state(session_id, None)
            .await?;
        if persisted_state.task_status.requires_reconciliation() {
            drop(session_lease);
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some(
                    "prompt rejected because the previous task has unreconciled side effects; run task reconciliation first"
                        .to_owned(),
                ),
            });
        }
        if let Some(active_task_id) = self.persisted_active_task(session_id).await? {
            let recovery = self
                .record_orphaned_task_recovery(
                    session_id,
                    active_task_id,
                    "session_lease_reacquired",
                )
                .await?;
            if recovery.reconciliation_required {
                drop(session_lease);
                return Ok(CommandAck {
                    command_id: command.command_id,
                    accepted: false,
                    reason: Some(
                        "prompt rejected because recovery found an uncertain side effect; run task reconciliation first"
                            .to_owned(),
                    ),
                });
            }
        }

        let delegation_context = if delegated_payload {
            let admission = self
                .execution
                .delegation_admissions
                .lock()
                .await
                .remove(&session_id)
                .ok_or_else(|| {
                    ClientError::TaskExecution(
                        "delegated child admission context disappeared before task start"
                            .to_owned(),
                    )
                })?;
            payload[crate::delegation::DELEGATED_TASK_MARKER] = Value::Bool(true);
            if let Some(payload) = payload.as_object_mut() {
                payload.remove(crate::delegation::DELEGATED_ADMISSION_TOKEN_KEY);
            }
            let context = admission.into_context();
            payload["_delegation"] = context.metadata();
            Some(context)
        } else {
            None
        };
        let provider_config_paths = self.provider_config_paths.clone();
        let provider_route_cache = Arc::clone(&self.execution.provider_route_cache);
        payload = run_blocking(move || {
            let mut cache = provider_route_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pin_provider_turn_settings_cached(
                &mut cache,
                provider_config_paths.as_ref(),
                &mut payload,
            );
            payload
        })
        .await?;
        self.upsert_current_thread(session_id, &payload).await?;
        let mut lane_manager = self.execution.lane_manager.lock().await;
        let transition = lane_manager.start_task(
            self.workspace_id,
            session_id,
            task_id,
            turn_id,
            command.actor.clone(),
            self.next_sequence_no(),
        )?;
        drop(lane_manager);
        let mut task_created =
            with_command_payload(transition.event, command.command_id, payload.clone());
        task_created.payload["execution_capabilities"] =
            self.execution_capabilities(requested_network, yolo);
        task_created.payload["run_provenance"] =
            serde_json::to_value(self.capture_run_provenance(task_id))?;
        if let Err(error) = self.record_event(task_created).await {
            let _ = self.execution.lane_manager.lock().await.finish_task(
                session_id,
                TaskStatus::Failed,
                self.next_sequence_no(),
            );
            return Err(error);
        }
        let spawn = Box::pin(self.clone().spawn_agent_task(
            HostedAgentTask {
                session_id,
                task_id,
                turn_id,
                payload,
            },
            session_lease,
            Vec::new(),
            delegation_context.map(DelegationContextSeed::Live),
            AgentGovernorUsage::default(),
        ));
        spawn.await?;

        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some(format!("started task {task_id} in session {session_id}")),
        })
    }

    async fn handle_update_queued_turn(
        &self,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let Some(turn_id) = queued_turn_id_from_payload(&command.payload) else {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some("queued turn update requires a valid turn_id".to_owned()),
            });
        };
        let prompt = prompt_from_payload(&command.payload);
        if prompt.trim().is_empty() {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some("queued turn update requires a non-empty prompt".to_owned()),
            });
        }
        let Some((task_id, lane)) = self.owned_active_lane(session_id, &command.actor).await else {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some("queued turn update requires control of an active task".to_owned()),
            });
        };
        let Some(mut payload) = self
            .latest_queued_turn_payload(session_id, task_id, turn_id)
            .await?
        else {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some("queued turn is no longer pending".to_owned()),
            });
        };
        payload["prompt"] = Value::String(prompt);
        if let Some(attachments) = command.payload.get("attachments") {
            payload["attachments"] = attachments.clone();
        }
        if payload.get("_task_contract_origin").and_then(Value::as_str) == Some("legacy_adapter") {
            if let Some(object) = payload.as_object_mut() {
                object.remove("task_contract");
            }
            let updated_prompt = prompt_from_payload(&payload);
            let mut task_contract = task_contract_from_payload(&payload)?;
            LegacyTaskAdapter::new(&payload, &updated_prompt).apply_to(&mut task_contract);
            if payload
                .get("defer_external_verification")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                task_contract.require_objective_validation = false;
            }
            task_contract
                .validate()
                .map_err(ClientError::TaskExecution)?;
            payload["task_contract"] = serde_json::to_value(task_contract)?;
        }

        let mut event = host_event(
            self.next_sequence_no(),
            session_id,
            Some(task_id),
            RuntimeEventType::TurnUpdated,
            RuntimeEventSource::User,
            json!({
                "summary": "queued user turn updated",
                "command_id": command.command_id,
                "payload": payload,
                "runtime_lane": lane,
            }),
        );
        event.turn_id = Some(turn_id);
        let recovered = recovered_pending_turn_from_event(&event)?.ok_or_else(|| {
            ClientError::TaskExecution("queued turn update is invalid".to_owned())
        })?;
        let replacement = ConfiguredPendingAgentTurn {
            turn: recovered.pending,
            execution: recovered.execution,
        };
        let control = self
            .execution
            .task_controls
            .lock()
            .await
            .get(&session_id)
            .cloned();
        let Some(control) = control.filter(|control| control.task_id == task_id) else {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some("active task finished before the queued turn was updated".to_owned()),
            });
        };
        let mutation = match control
            .execution
            .reserve_configured_turn_update(turn_id, replacement)
        {
            Ok(mutation) => mutation,
            Err(AgentLoopError::PendingTurnNotFound) => {
                return Ok(CommandAck {
                    command_id: command.command_id,
                    accepted: false,
                    reason: Some("queued turn is no longer pending".to_owned()),
                });
            }
            Err(AgentLoopError::PendingTurnMutationInProgress) => {
                return Ok(CommandAck {
                    command_id: command.command_id,
                    accepted: false,
                    reason: Some("queued turn is already being changed".to_owned()),
                });
            }
            Err(error) => return Err(ClientError::TaskExecution(error.to_string())),
        };
        self.record_event(event).await?;
        mutation.commit();
        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some("queued turn updated".to_owned()),
        })
    }

    async fn handle_cancel_queued_turn(
        &self,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let Some(turn_id) = queued_turn_id_from_payload(&command.payload) else {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some("queued turn cancellation requires a valid turn_id".to_owned()),
            });
        };
        let Some((task_id, mut lane)) = self.owned_active_lane(session_id, &command.actor).await
        else {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some(
                    "queued turn cancellation requires control of an active task".to_owned(),
                ),
            });
        };
        let control = self
            .execution
            .task_controls
            .lock()
            .await
            .get(&session_id)
            .cloned();
        let Some(control) = control.filter(|control| control.task_id == task_id) else {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some(
                    "active task finished before the queued turn was cancelled".to_owned(),
                ),
            });
        };
        let mutation = match control.execution.reserve_turn_cancellation(turn_id) {
            Ok(mutation) => mutation,
            Err(AgentLoopError::PendingTurnNotFound) => {
                return Ok(CommandAck {
                    command_id: command.command_id,
                    accepted: false,
                    reason: Some("queued turn is no longer pending".to_owned()),
                });
            }
            Err(AgentLoopError::PendingTurnMutationInProgress) => {
                return Ok(CommandAck {
                    command_id: command.command_id,
                    accepted: false,
                    reason: Some("queued turn is already being changed".to_owned()),
                });
            }
            Err(error) => return Err(ClientError::TaskExecution(error.to_string())),
        };
        lane.pending_turns.retain(|pending| *pending != turn_id);
        let mut event = host_event(
            self.next_sequence_no(),
            session_id,
            Some(task_id),
            RuntimeEventType::TurnCancelled,
            RuntimeEventSource::User,
            json!({
                "summary": "queued user turn cancelled",
                "command_id": command.command_id,
                "turn_id": turn_id,
                "runtime_lane": lane,
            }),
        );
        event.turn_id = Some(turn_id);
        self.record_event(event).await?;
        mutation.commit();
        self.execution
            .lane_manager
            .lock()
            .await
            .discard_queued_turn(session_id, turn_id)?;
        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some("queued turn cancelled".to_owned()),
        })
    }

    async fn owned_active_lane(
        &self,
        session_id: SessionId,
        actor: &Actor,
    ) -> Option<(TaskId, golutra_core::RuntimeLane)> {
        self.execution
            .lane_manager
            .lock()
            .await
            .lane(session_id)
            .filter(|lane| is_active_status(lane.status) && lane.active_controller == *actor)
            .map(|lane| (lane.task_id, lane.clone()))
    }

    async fn latest_queued_turn_payload(
        &self,
        session_id: SessionId,
        task_id: TaskId,
        turn_id: TurnId,
    ) -> Result<Option<Value>, ClientError> {
        let events = self
            .storage
            .repositories
            .events
            .load(session_id, Some(task_id), None)
            .await?;
        let mut payload = None;
        for event in events.iter().filter(|event| event.turn_id == Some(turn_id)) {
            match event.event_type {
                RuntimeEventType::TurnQueued | RuntimeEventType::TurnUpdated => {
                    payload = event.payload.get("payload").cloned();
                }
                RuntimeEventType::TurnStarted | RuntimeEventType::TurnCancelled => {
                    payload = None;
                }
                _ => {}
            }
        }
        Ok(payload)
    }

    async fn handle_reconcile_task_command(
        self: &Arc<Self>,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        const MAX_RECONCILIATION_NOTE_CHARS: usize = 4_096;

        let command_id = command.command_id;
        let state = self
            .storage
            .repositories
            .projections
            .state(session_id, None)
            .await?;
        if state.task_status != TaskStatus::Uncertain {
            return Ok(CommandAck {
                command_id,
                accepted: false,
                reason: Some("task reconciliation requires an uncertain session state".to_owned()),
            });
        }
        let task_id = match command.payload.get("task_id").and_then(Value::as_str) {
            Some(value) => match value.parse::<TaskId>() {
                Ok(task_id) => task_id,
                Err(_) => {
                    return Ok(CommandAck {
                        command_id,
                        accepted: false,
                        reason: Some("task_id is invalid".to_owned()),
                    });
                }
            },
            None => match state.active_task_id {
                Some(task_id) => task_id,
                None => {
                    return Ok(CommandAck {
                        command_id,
                        accepted: false,
                        reason: Some("uncertain session has no active task id".to_owned()),
                    });
                }
            },
        };
        if state.active_task_id != Some(task_id) {
            return Ok(CommandAck {
                command_id,
                accepted: false,
                reason: Some("task_id is not the session's uncertain task".to_owned()),
            });
        }
        let decision = match command.payload.get("decision").cloned() {
            Some(value) => match serde_json::from_value::<TaskReconciliationDecision>(value) {
                Ok(decision) => decision,
                Err(_) => {
                    return Ok(CommandAck {
                        command_id,
                        accepted: false,
                        reason: Some(
                            "decision must be no_side_effect_observed, side_effect_observed, or abandon"
                                .to_owned(),
                        ),
                    });
                }
            },
            None => {
                return Ok(CommandAck {
                    command_id,
                    accepted: false,
                    reason: Some("reconciliation decision is required".to_owned()),
                });
            }
        };
        let note = command
            .payload
            .get("note")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|note| !note.is_empty())
            .map(ToOwned::to_owned);
        if note
            .as_ref()
            .is_some_and(|note| note.chars().count() > MAX_RECONCILIATION_NOTE_CHARS)
        {
            return Ok(CommandAck {
                command_id,
                accepted: false,
                reason: Some(format!(
                    "reconciliation note exceeds {MAX_RECONCILIATION_NOTE_CHARS} characters"
                )),
            });
        }

        let session_lease = match self.try_acquire_session_lease(session_id)? {
            SessionLeaseAttempt::Acquired(lease) => lease,
            SessionLeaseAttempt::Busy => {
                return Ok(CommandAck {
                    command_id,
                    accepted: false,
                    reason: Some(
                        "task reconciliation rejected because the session is owned by another runtime process"
                            .to_owned(),
                    ),
                });
            }
        };
        let events = self
            .storage
            .repositories
            .events
            .load(session_id, Some(task_id), None)
            .await?;
        let Some(recovery_event) = events
            .iter()
            .rev()
            .find(|event| event.event_type == RuntimeEventType::TaskUncertain)
        else {
            drop(session_lease);
            return Ok(CommandAck {
                command_id,
                accepted: false,
                reason: Some("uncertain task has no durable recovery record".to_owned()),
            });
        };
        if events.iter().any(|event| {
            event.sequence_no > recovery_event.sequence_no
                && event.event_type == RuntimeEventType::TaskReconciled
        }) {
            drop(session_lease);
            return Ok(CommandAck {
                command_id,
                accepted: false,
                reason: Some("task recovery was already reconciled".to_owned()),
            });
        }
        let pending_turns = self
            .recoverable_pending_turns(session_id, Some(task_id))
            .await?;
        let resulting_status = match decision {
            TaskReconciliationDecision::Abandon => TaskStatus::Cancelled,
            TaskReconciliationDecision::NoSideEffectObserved
            | TaskReconciliationDecision::SideEffectObserved => TaskStatus::Interrupted,
        };
        let record = TaskReconciliationRecord {
            task_id,
            recovery_event_ref: recovery_event.id,
            decision,
            resulting_status,
            note,
            reconciled_by: command.actor,
            reconciled_at: chrono::Utc::now(),
            resumed_pending_turns: !pending_turns.is_empty(),
        };
        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            Some(task_id),
            RuntimeEventType::TaskReconciled,
            RuntimeEventSource::Runtime,
            json!({
                "summary": "uncertain task recovery was explicitly reconciled",
                "status": resulting_status,
                "record": record,
                "command_id": command_id,
            }),
        ))
        .await?;
        if !pending_turns.is_empty() {
            self.clone()
                .restart_pending_turns(session_id, pending_turns, session_lease)
                .await?;
        } else {
            drop(session_lease);
        }
        Ok(CommandAck {
            command_id,
            accepted: true,
            reason: Some(format!(
                "task {task_id} recovery reconciled as {resulting_status:?}"
            )),
        })
    }

    async fn handle_lane_command(
        &self,
        session_id: SessionId,
        command: &SessionCommand,
        action: &str,
    ) -> Result<CommandAck, ClientError> {
        let command_id = command.command_id;
        let task_control = self
            .execution
            .task_controls
            .lock()
            .await
            .get(&session_id)
            .cloned();
        let Some(task_control) = task_control else {
            let active_task_id = self.persisted_active_task(session_id).await?;
            if action == "abort"
                && let Some(active_task_id) = active_task_id
            {
                let session_lease = match self.try_acquire_session_lease(session_id)? {
                    SessionLeaseAttempt::Acquired(lease) => lease,
                    SessionLeaseAttempt::Busy => {
                        return Ok(CommandAck {
                            command_id,
                            accepted: false,
                            reason: Some(
                                "abort rejected because the active task belongs to another runtime process"
                                    .to_owned(),
                            ),
                        });
                    }
                };
                self.record_orphaned_task_cancelled(
                    session_id,
                    Some(active_task_id),
                    "controller_abort_after_owner_exit",
                    "orphaned persisted task cancelled by controller",
                )
                .await?;
                drop(session_lease);
                return Ok(CommandAck {
                    command_id,
                    accepted: true,
                    reason: Some("orphaned persisted task cancelled".to_owned()),
                });
            }
            return Ok(CommandAck {
                command_id,
                accepted: false,
                reason: Some(active_task_id.map_or_else(
                    || format!("{action} rejected because the session has no active task"),
                    |_| {
                        format!(
                            "{action} rejected because the active task belongs to another runtime process"
                        )
                    },
                )),
            });
        };
        if task_control.abort_handle.is_finished() {
            return Ok(CommandAck {
                command_id,
                accepted: false,
                reason: Some(format!(
                    "{action} rejected because the task already finished"
                )),
            });
        }
        let mut lane_manager = self.execution.lane_manager.lock().await;
        if lane_manager
            .lane(session_id)
            .is_some_and(|lane| lane.active_controller != command.actor)
        {
            return Ok(CommandAck {
                command_id,
                accepted: false,
                reason: Some(format!(
                    "{action} rejected because the actor is not the active controller"
                )),
            });
        }
        let transition = match action {
            "abort" => lane_manager.abort(session_id, self.next_sequence_no()),
            "pause" => lane_manager.pause(session_id, self.next_sequence_no()),
            "resume" => lane_manager.resume(session_id, self.next_sequence_no()),
            _ => unreachable!("lane action is constrained by caller"),
        };
        drop(lane_manager);
        match transition {
            Ok(transition) => {
                self.record_event(with_command_payload(
                    transition.event,
                    command_id,
                    json!({ "action": action }),
                ))
                .await?;
                match action {
                    "abort" => task_control.execution.cancel(),
                    "pause" => task_control.execution.pause(),
                    "resume" => task_control.execution.resume(),
                    _ => unreachable!("lane action is constrained by caller"),
                }
            }
            Err(RuntimeLaneError::LaneNotFound) => {
                return Ok(CommandAck {
                    command_id,
                    accepted: false,
                    reason: Some(format!(
                        "{action} rejected because the runtime lane is not in a compatible active state"
                    )),
                });
            }
            Err(error) => return Err(error.into()),
        }
        Ok(CommandAck {
            command_id,
            accepted: true,
            reason: Some(format!("{action} accepted in session {session_id}")),
        })
    }

    async fn handle_provider_configured_command(
        &self,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let paths = self
            .provider_config_paths
            .clone()
            .map_or_else(ProviderConfigPaths::global, Ok)
            .map_err(|error| ClientError::TaskExecution(error.to_string()))?;
        let (environment, redacted) = run_blocking(move || {
            let environment = load_provider_runtime_env_from_paths(&paths)
                .map_err(|error| ClientError::TaskExecution(error.to_string()))?;
            let redacted = ConfiguredProvider::redacted_from_reader(|key| environment.get(key))
                .map_err(|error| ClientError::TaskExecution(error.to_string()))?;
            Ok::<_, ClientError>((environment, redacted))
        })
        .await??;
        let protocol = redacted.protocol;
        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            None,
            RuntimeEventType::ProviderConfigured,
            RuntimeEventSource::Runtime,
            json!({
                "summary": "provider configuration reloaded by runtime host",
                "command_id": command.command_id,
                "provider": redacted,
            }),
        ))
        .await?;
        let should_probe = command.kind == SessionCommandKind::ProviderAuthSubmitted
            || command
                .payload
                .get("probe")
                .and_then(Value::as_bool)
                .unwrap_or(false);
        if should_probe {
            self.record_event(host_event(
                self.next_sequence_no(),
                session_id,
                None,
                RuntimeEventType::ProviderProbeStarted,
                RuntimeEventSource::Provider,
                json!({
                    "summary": "provider capability probe started",
                    "command_id": command.command_id,
                }),
            ))
            .await?;
            let probe = ConfiguredProvider::probe_from_reader_with_credential(
                |key| environment.get(key),
                environment.credential_provider(),
            )
            .await;
            let probe = match probe {
                Ok(probe) => probe,
                Err(error) => {
                    let event_type = if error.is_rate_limited() {
                        RuntimeEventType::ProviderRateLimited
                    } else {
                        RuntimeEventType::ProviderAuthFailed
                    };
                    self.record_event(host_event(
                        self.next_sequence_no(),
                        session_id,
                        None,
                        event_type,
                        RuntimeEventSource::Provider,
                        json!({
                            "summary": "provider capability probe failed",
                            "command_id": command.command_id,
                            "error": error.to_string(),
                        }),
                    ))
                    .await?;
                    return Ok(CommandAck {
                        command_id: command.command_id,
                        accepted: false,
                        reason: Some(error.to_string()),
                    });
                }
            };
            self.record_event(host_event(
                self.next_sequence_no(),
                session_id,
                None,
                RuntimeEventType::ProviderProbeCompleted,
                RuntimeEventSource::Provider,
                json!({
                    "summary": "provider capability probe completed",
                    "command_id": command.command_id,
                    "probe": probe,
                }),
            ))
            .await?;
        } else {
            self.record_event(host_event(
                self.next_sequence_no(),
                session_id,
                None,
                RuntimeEventType::ProviderProbeCompleted,
                RuntimeEventSource::Provider,
                json!({
                    "summary": "provider installation was already verified",
                    "command_id": command.command_id,
                    "capabilities": protocol_capabilities(protocol),
                    "source": "verified_install",
                }),
            ))
            .await?;
        }

        // Provider auth/configuration may have changed without changing the
        // task payload. Drop the route snapshot so the next turn observes the
        // newly verified credential and endpoint immediately.
        self.execution
            .provider_route_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();

        let requested_id = provider_auth_request_id_from_payload(&command.payload)?;
        let pending = {
            let mut waiters = self.execution.provider_auth_waiters.lock().await;
            if let Some(pending) = waiters.get(&session_id)
                && requested_id.is_some_and(|request_id| request_id != pending.request_id)
            {
                return Ok(CommandAck {
                    command_id: command.command_id,
                    accepted: false,
                    reason: Some(
                        "provider auth request id does not match the active request".to_owned(),
                    ),
                });
            }
            waiters.remove(&session_id)
        };
        if let Some(pending) = pending {
            let mut transition = self
                .execution
                .lane_manager
                .lock()
                .await
                .authentication_resolved(session_id, self.next_sequence_no())?;
            transition.event.payload["summary"] =
                json!("provider authentication submitted and verified");
            transition.event.payload["request_id"] = json!(pending.request_id);
            transition.event.payload["command_id"] = json!(command.command_id);
            transition.event.payload["runtime_lane"] = json!(transition.lane);
            self.record_event(transition.event).await?;
            let _ = pending.resolution.send(ProviderAuthResolution::Submitted);
        }
        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some("provider configuration loaded and verified".to_owned()),
        })
    }

    async fn handle_provider_auth_cancelled_command(
        &self,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let requested_id = provider_auth_request_id_from_payload(&command.payload)?;
        let pending = {
            let mut waiters = self.execution.provider_auth_waiters.lock().await;
            let Some(active) = waiters.get(&session_id) else {
                return Ok(CommandAck {
                    command_id: command.command_id,
                    accepted: false,
                    reason: Some("session has no pending provider auth request".to_owned()),
                });
            };
            if requested_id.is_some_and(|request_id| request_id != active.request_id) {
                return Ok(CommandAck {
                    command_id: command.command_id,
                    accepted: false,
                    reason: Some(
                        "provider auth request id does not match the active request".to_owned(),
                    ),
                });
            }
            waiters.remove(&session_id).expect("checked pending auth")
        };
        let lane = self
            .execution
            .lane_manager
            .lock()
            .await
            .lane(session_id)
            .cloned();
        let mut event = host_event(
            self.next_sequence_no(),
            session_id,
            lane.as_ref().map(|lane| lane.task_id),
            RuntimeEventType::ProviderAuthCancelled,
            RuntimeEventSource::User,
            json!({
                "summary": "provider authentication was cancelled",
                "request_id": pending.request_id,
                "command_id": command.command_id,
            }),
        );
        event.turn_id = lane.and_then(|lane| lane.active_turn_id);
        self.record_event(event).await?;
        let _ = pending.resolution.send(ProviderAuthResolution::Cancelled);
        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some("provider authentication cancelled".to_owned()),
        })
    }

    async fn handle_takeover_command(
        &self,
        session_id: SessionId,
        command: &SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        if !self
            .execution
            .task_controls
            .lock()
            .await
            .contains_key(&session_id)
        {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some(
                    "takeover rejected because the session has no locally active task".to_owned(),
                ),
            });
        }
        let transition = self.execution.lane_manager.lock().await.takeover(
            session_id,
            command.actor.clone(),
            self.next_sequence_no(),
        );
        match transition {
            Ok(transition) => {
                self.record_event(with_command_payload(
                    transition.event,
                    command.command_id,
                    json!({"action": "takeover"}),
                ))
                .await?;
                Ok(CommandAck {
                    command_id: command.command_id,
                    accepted: true,
                    reason: Some("active runtime controller transferred".to_owned()),
                })
            }
            Err(RuntimeLaneError::LaneNotFound) => Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some("takeover rejected because the session has no active task".to_owned()),
            }),
            Err(error) => Err(error.into()),
        }
    }

    async fn handle_thread_metadata_command(
        &self,
        attached_session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let Some(thread_id) = command
            .payload
            .get("thread_id")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<ThreadId>().ok())
        else {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some("thread_id is required".to_owned()),
            });
        };
        let Some(mut thread) = self.storage.repositories.threads.by_id(thread_id).await? else {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some(format!("thread {thread_id} was not found")),
            });
        };
        self.ensure_thread_in_workspace(&thread)?;
        let purge_requested = command.kind == SessionCommandKind::DeleteThread
            && command.payload.get("purge") == Some(&Value::Bool(true));
        if purge_requested
            && command.payload.get("confirm").and_then(Value::as_str) != Some("PURGE")
        {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some("privacy purge requires confirm=PURGE".to_owned()),
            });
        }
        let (event_type, summary) = match command.kind {
            SessionCommandKind::RenameThread => (RuntimeEventType::ThreadRenamed, "thread renamed"),
            SessionCommandKind::ArchiveThread => {
                (RuntimeEventType::ThreadArchived, "thread archived")
            }
            SessionCommandKind::DeleteThread if purge_requested => (
                RuntimeEventType::ThreadDeleted,
                "thread purged; audit tombstone retained",
            ),
            SessionCommandKind::DeleteThread => (
                RuntimeEventType::ThreadDeleted,
                "thread removed from history",
            ),
            _ => unreachable!("thread metadata handler only receives thread commands"),
        };
        if let Some(event) = self
            .existing_thread_metadata_event(&thread, event_type, command.command_id)
            .await?
        {
            if purge_requested {
                self.remove_thread_rollout_projection(thread.thread_id)
                    .await?;
            } else {
                self.rebuild_thread_rollout(&thread).await?;
            }
            self.publish_live_event(event);
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: true,
                reason: Some(summary.to_owned()),
            });
        }
        self.ensure_thread_not_removed(&thread)?;
        if matches!(
            command.kind,
            SessionCommandKind::ArchiveThread | SessionCommandKind::DeleteThread
        ) && attached_session_id == thread.session_id
        {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some(
                    "the currently attached session cannot be archived or deleted".to_owned(),
                ),
            });
        }
        if self
            .execution
            .lane_manager
            .lock()
            .await
            .lane(thread.session_id)
            .is_some_and(|lane| is_active_status(lane.status))
        {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some("active sessions cannot be renamed, archived, or deleted".to_owned()),
            });
        }
        let _session_lease = match self.try_acquire_session_lease(thread.session_id)? {
            SessionLeaseAttempt::Acquired(lease) => lease,
            SessionLeaseAttempt::Busy => {
                return Ok(CommandAck {
                    command_id: command.command_id,
                    accepted: false,
                    reason: Some(
                        "thread metadata cannot change while the session is active in another Golutra runtime process"
                            .to_owned(),
                    ),
                });
            }
        };
        let mutation = match command.kind {
            SessionCommandKind::RenameThread => {
                let Some(title) = command
                    .payload
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|title| !title.is_empty() && title.chars().count() <= 256)
                else {
                    return Ok(CommandAck {
                        command_id: command.command_id,
                        accepted: false,
                        reason: Some("title must contain between 1 and 256 characters".to_owned()),
                    });
                };
                thread.title = title.to_owned();
                thread.updated_at = chrono::Utc::now();
                ThreadMetadataMutation::Upsert
            }
            SessionCommandKind::ArchiveThread => {
                thread.archived = true;
                thread.updated_at = chrono::Utc::now();
                ThreadMetadataMutation::Upsert
            }
            SessionCommandKind::DeleteThread => {
                thread.archived = true;
                thread.removed = true;
                thread.updated_at = chrono::Utc::now();
                ThreadMetadataMutation::Delete
            }
            _ => unreachable!("thread metadata handler only receives thread commands"),
        };
        let event = host_event(
            self.next_sequence_no(),
            thread.session_id,
            None,
            event_type,
            RuntimeEventSource::User,
            json!({
                "summary": summary,
                "thread_id": thread.thread_id,
                "actor": command.actor,
                "command_id": command.command_id.to_string(),
            }),
        );
        if !self
            .record_thread_metadata_event(&thread, mutation, event)
            .await?
        {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some(format!("thread {thread_id} was not found")),
            });
        }
        if purge_requested {
            self.remove_thread_rollout_projection(thread.thread_id)
                .await?;
        }
        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some(summary.to_owned()),
        })
    }

    pub(super) async fn archive_thread_after_delegated_parent_cancel(
        &self,
        attached_session_id: SessionId,
        thread_id: ThreadId,
        actor: &Actor,
    ) -> Result<(), ClientError> {
        let Some(mut thread) = self.storage.repositories.threads.by_id(thread_id).await? else {
            return Ok(());
        };
        self.ensure_thread_in_workspace(&thread)?;
        if thread.archived {
            return Ok(());
        }
        if attached_session_id == thread.session_id {
            return Err(ClientError::TaskExecution(
                "a delegated child cannot archive its attached session".to_owned(),
            ));
        }
        if self
            .execution
            .lane_manager
            .lock()
            .await
            .lane(thread.session_id)
            .is_some_and(|lane| is_active_status(lane.status))
        {
            return Err(ClientError::TaskExecution(
                "a delegated child must reach a terminal lane before archival".to_owned(),
            ));
        }
        let _session_lease = match self.try_acquire_session_lease(thread.session_id)? {
            SessionLeaseAttempt::Acquired(lease) => lease,
            SessionLeaseAttempt::Busy => {
                return Err(ClientError::TaskExecution(
                    "delegated child archival is owned by another runtime process".to_owned(),
                ));
            }
        };
        thread.archived = true;
        thread.updated_at = chrono::Utc::now();
        let event = host_event(
            self.next_sequence_no(),
            thread.session_id,
            None,
            RuntimeEventType::ThreadArchived,
            RuntimeEventSource::Runtime,
            json!({
                "summary": "delegated child thread archived after parent cancellation",
                "thread_id": thread.thread_id,
                "actor": actor,
                "recovery": "delegated_parent_cancelled",
            }),
        );
        if !self
            .record_thread_metadata_event(&thread, ThreadMetadataMutation::Upsert, event)
            .await?
        {
            return Err(ClientError::TaskExecution(format!(
                "delegated child thread {thread_id} disappeared during archival"
            )));
        }
        Ok(())
    }

    async fn existing_thread_metadata_event(
        &self,
        thread: &golutra_store::ThreadRecord,
        event_type: RuntimeEventType,
        command_id: CommandId,
    ) -> Result<Option<RuntimeEvent>, ClientError> {
        let command_id = command_id.to_string();
        let thread_id = thread.thread_id.to_string();
        Ok(self
            .storage
            .repositories
            .events
            .load(thread.session_id, None, None)
            .await?
            .into_iter()
            .rev()
            .find(|event| {
                event.event_type == event_type
                    && event.payload.get("command_id").and_then(Value::as_str)
                        == Some(command_id.as_str())
                    && event.payload.get("thread_id").and_then(Value::as_str)
                        == Some(thread_id.as_str())
            }))
    }

    async fn remove_thread_rollout_projection(
        &self,
        thread_id: ThreadId,
    ) -> Result<(), ClientError> {
        let (Some(workspace_root), Some(paths)) =
            (self.workspace_root.clone(), self.runtime_paths.clone())
        else {
            return Ok(());
        };
        let rollout_path = rollout_path_for_workspace(&paths, &workspace_root, thread_id);
        run_blocking(move || remove_rollout_projection(&rollout_path)).await??;
        Ok(())
    }

    async fn record_thread_metadata_event(
        &self,
        thread: &golutra_store::ThreadRecord,
        mutation: ThreadMetadataMutation,
        event: RuntimeEvent,
    ) -> Result<bool, ClientError> {
        let _writer = self.execution.event_writer.lock().await;
        let causal_before = self.execution.causal_ledger.lock().await.clone();
        let event = match self.prepare_canonical_event(event).await {
            Ok(event) => event,
            Err(error) => {
                *self.execution.causal_ledger.lock().await = causal_before;
                return Err(error);
            }
        };
        let committed = match mutation {
            ThreadMetadataMutation::Upsert => self
                .storage
                .repositories
                .threads
                .upsert_with_event(thread, event)
                .await
                .map(Some),
            ThreadMetadataMutation::Delete => {
                self.storage
                    .repositories
                    .threads
                    .delete_with_event(thread.thread_id, event)
                    .await
            }
        };
        let Some(event) = (match committed {
            Ok(event) => event,
            Err(error) => {
                *self.execution.causal_ledger.lock().await = causal_before;
                return Err(error.into());
            }
        }) else {
            *self.execution.causal_ledger.lock().await = causal_before;
            return Ok(false);
        };
        self.publish_committed_event(event).await?;
        Ok(true)
    }

    async fn handle_approval_command(
        &self,
        session_id: SessionId,
        command: SessionCommand,
        decision: ApprovalDecision,
    ) -> Result<CommandAck, ClientError> {
        if self
            .execution
            .lane_manager
            .lock()
            .await
            .lane(session_id)
            .is_some_and(|lane| lane.active_controller != command.actor)
        {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some(
                    "approval rejected because the actor is not the active controller".to_owned(),
                ),
            });
        }
        let state = self
            .storage
            .repositories
            .projections
            .state(session_id, None)
            .await?;
        let pending_approval = state
            .pending_approval
            .as_deref()
            .and_then(|value| value.parse::<ApprovalId>().ok());
        let requested_approval = match command.payload.get("approval_id") {
            None | Some(Value::Null) => pending_approval,
            Some(Value::String(value)) => match value.parse::<ApprovalId>() {
                Ok(approval_id) => Some(approval_id),
                Err(_) => {
                    return Ok(CommandAck {
                        command_id: command.command_id,
                        accepted: false,
                        reason: Some("approval_id must be a valid UUID".to_owned()),
                    });
                }
            },
            Some(_) => {
                return Ok(CommandAck {
                    command_id: command.command_id,
                    accepted: false,
                    reason: Some("approval_id must be a valid UUID".to_owned()),
                });
            }
        };
        let Some(approval_id) = requested_approval else {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some("session has no pending approval".to_owned()),
            });
        };
        if pending_approval != Some(approval_id) {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some(format!(
                    "approval {approval_id} is not pending in this session"
                )),
            });
        }
        let scope = command
            .payload
            .get("scope")
            .cloned()
            .map(serde_json::from_value::<ApprovalScope>)
            .transpose()
            .map_err(|_| {
                ClientError::TaskExecution(
                    "approval scope must be once, resource_prefix, or session".to_owned(),
                )
            })?
            .unwrap_or_default();
        let resource_prefix = match command.payload.get("resource_prefix") {
            None | Some(Value::Null) => None,
            Some(Value::String(prefix))
                if !prefix.is_empty() && prefix.chars().count() <= 2_048 =>
            {
                Some(prefix.clone())
            }
            Some(_) => {
                return Ok(CommandAck {
                    command_id: command.command_id,
                    accepted: false,
                    reason: Some(
                        "approval resource_prefix must contain between 1 and 2048 characters"
                            .to_owned(),
                    ),
                });
            }
        };
        if scope == ApprovalScope::ResourcePrefix {
            let events = self
                .storage
                .repositories
                .events
                .load(session_id, state.active_task_id, None)
                .await?;
            let request = events.iter().rev().find_map(|event| {
                (event.event_type == RuntimeEventType::ApprovalRequested
                    && event
                        .payload
                        .get("approval_id")
                        .and_then(Value::as_str)
                        .and_then(|value| value.parse::<ApprovalId>().ok())
                        == Some(approval_id))
                .then(|| event.payload.get("request").cloned())
                .flatten()
                .and_then(|value| serde_json::from_value::<ApprovalRequest>(value).ok())
            });
            let valid = request.as_ref().is_some_and(|request| {
                resource_prefix.as_deref().is_some_and(|prefix| {
                    approval_resource_matches(&request.tool_name, prefix, &request.resource)
                })
            });
            if !valid {
                return Ok(CommandAck {
                    command_id: command.command_id,
                    accepted: false,
                    reason: Some(
                        "approval resource_prefix must prefix the pending resource".to_owned(),
                    ),
                });
            }
        } else if resource_prefix.is_some() {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some(
                    "approval resource_prefix is only valid for resource_prefix scope".to_owned(),
                ),
            });
        }
        let control = self
            .execution
            .task_controls
            .lock()
            .await
            .get(&session_id)
            .cloned();
        let Some(control) = control else {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some("active task control is unavailable".to_owned()),
            });
        };
        control
            .execution
            .resolve_approval(ApprovalResolution {
                approval_id,
                decision,
                scope: if decision == ApprovalDecision::Approved {
                    scope
                } else {
                    ApprovalScope::Once
                },
                resource_prefix: (decision == ApprovalDecision::Approved)
                    .then_some(resource_prefix)
                    .flatten(),
                reason: format!("resolved by {}", command.actor.id),
            })
            .await
            .map_err(|error| ClientError::TaskExecution(error.to_string()))?;

        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some(format!("approval {approval_id} resolved as {decision:?}")),
        })
    }

    async fn handle_answer_question_command(
        &self,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let Some((task_id, _)) = self.owned_active_lane(session_id, &command.actor).await else {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some(
                    "answer rejected because the actor does not control an active task".to_owned(),
                ),
            });
        };
        let events = self
            .storage
            .repositories
            .events
            .load(session_id, Some(task_id), None)
            .await?;
        let pending = pending_user_question(&events, Some(task_id));
        let Some(request) = pending else {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some("session has no pending structured question".to_owned()),
            });
        };
        let requested_id = match command.payload.get("question_id") {
            None | Some(Value::Null) => request.question_id,
            Some(Value::String(value)) => match value.parse::<QuestionId>() {
                Ok(question_id) => question_id,
                Err(_) => {
                    return Ok(CommandAck {
                        command_id: command.command_id,
                        accepted: false,
                        reason: Some("question_id must be a valid UUID".to_owned()),
                    });
                }
            },
            Some(_) => {
                return Ok(CommandAck {
                    command_id: command.command_id,
                    accepted: false,
                    reason: Some("question_id must be a valid UUID".to_owned()),
                });
            }
        };
        if requested_id != request.question_id {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some(format!(
                    "question {requested_id} is not pending in this session"
                )),
            });
        }
        let answers = command
            .payload
            .get("answers")
            .cloned()
            .map(serde_json::from_value::<Vec<UserQuestionAnswer>>)
            .transpose()
            .map_err(|error| {
                ClientError::TaskExecution(format!("invalid structured answers: {error}"))
            })?
            .unwrap_or_default();
        let resolution = UserQuestionResolution {
            question_id: request.question_id,
            answers,
            reason: format!("answered by {}", command.actor.id),
        };
        if let Err(error) = request.validate_resolution(&resolution) {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some(error),
            });
        }
        let control = self
            .execution
            .task_controls
            .lock()
            .await
            .get(&session_id)
            .filter(|control| control.task_id == task_id)
            .cloned();
        let Some(control) = control else {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some("active task control is unavailable".to_owned()),
            });
        };
        control
            .execution
            .resolve_question(resolution)
            .await
            .map_err(|error| ClientError::TaskExecution(error.to_string()))?;
        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some(format!("question {} answered", request.question_id)),
        })
    }

    async fn handle_compact_command(
        &self,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        if self
            .execution
            .lane_manager
            .lock()
            .await
            .lane(session_id)
            .is_some_and(|lane| lane.active_controller != command.actor)
        {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some(
                    "compaction rejected because the actor is not the active controller".to_owned(),
                ),
            });
        }
        // 复用模型历史缓存，避免显式压缩把整个 session（含 telemetry）
        // 一次性物化；缓存已保留最新压缩边界和有界的对话尾部。
        let events = self.cached_history_events(session_id).await?;
        let latest_compaction = events.iter().rev().find_map(context_compaction_from_event);
        let compacted_after = latest_compaction
            .as_ref()
            .map(|(sequence_no, _)| *sequence_no)
            .unwrap_or_default();
        let history = effective_model_history_events(
            events
                .iter()
                .filter(|event| event.sequence_no > compacted_after),
        )
        .into_iter()
        .filter_map(|event| {
            let contributor = conversation_history_contributor(event)?;
            let fallback_line = conversation_history_line(event)?;
            Some((
                ProviderMessage {
                    role: contributor.role,
                    content: contributor.content,
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: Vec::new(),
                    metadata: Default::default(),
                },
                fallback_line,
            ))
        })
        .collect::<Vec<_>>();
        if history.is_empty() {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some("session has no new conversation history to compact".to_owned()),
            });
        }
        let previous_summary = latest_compaction
            .as_ref()
            .and_then(|(_, content)| parse_compaction_summary_envelope(content))
            .map(|envelope| envelope.summary);
        let lines = history
            .iter()
            .map(|(_, line)| line.clone())
            .collect::<Vec<_>>();
        let source_messages = history
            .iter()
            .map(|(message, _)| message.clone())
            .collect::<Vec<_>>();
        let source_range = CompactionSourceRange {
            start: 0,
            end: u64::try_from(source_messages.len()).unwrap_or(u64::MAX),
        };
        let source_tokens = estimate_message_tokens(&source_messages);
        let source_checksum = compaction_source_checksum(&source_messages);
        let fallback = deterministic_compaction_fallback(
            previous_summary.as_deref(),
            &lines,
            EXPLICIT_COMPACTION_TOKEN_BUDGET,
        );
        let mut summary = compaction_summary_envelope(
            &fallback,
            source_range.clone(),
            source_tokens,
            source_checksum.clone(),
            EXPLICIT_COMPACTION_TOKEN_BUDGET,
        );
        let active_task_id = self
            .storage
            .repositories
            .projections
            .state(session_id, None)
            .await?
            .active_task_id;
        let trace_ids = active_task_id
            .or_else(|| events.iter().rev().find_map(|event| event.task_id))
            .zip(events.iter().rev().find_map(|event| event.turn_id));
        let mut strategy = "fallback_facts";
        if !self.force_mock_provider
            && let Some((task_id, turn_id)) = trace_ids
        {
            let prompt_cache_scope = self
                .prompt_cache_scope(session_id, false)
                .await?
                .compaction();
            let provider_config_paths = self.provider_config_paths.clone();
            let provider_route_cache = Arc::clone(&self.execution.provider_route_cache);
            let provider_plan = run_blocking(move || {
                let mut cache = provider_route_cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                cached_mock_provider_plan(
                    &mut cache,
                    provider_config_paths.as_ref(),
                    &json!({}),
                    "compact conversation history",
                )
            })
            .await?
            .ok();
            if let Some(provider_plan) = provider_plan {
                let contract = provider_plan.provider.contract();
                if contract.native_protocol != "in_memory"
                    && let Some(provider_request) = compaction_summary_request(
                        task_id,
                        turn_id,
                        &contract,
                        prompt_cache_scope,
                        previous_summary,
                        &source_messages,
                        EXPLICIT_COMPACTION_TOKEN_BUDGET,
                    )
                    && let Some(context_snapshot) = compaction_summary_context_snapshot(
                        &provider_plan.context_builder,
                        session_id,
                        &provider_request,
                    )
                {
                    let budget_snapshot_ref = context_snapshot.budget_snapshot.snapshot_id;
                    let request_id = provider_request.request_id;
                    let trace_task = HostedAgentTask {
                        session_id,
                        task_id,
                        turn_id,
                        payload: json!({}),
                    };
                    self.record_auxiliary_trace_observation(
                        &trace_task,
                        AgentLoopTraceEvent::ContextSnapshotCaptured {
                            snapshot: context_snapshot,
                            request: provider_request.clone(),
                        },
                    )
                    .await?;
                    self.record_auxiliary_trace_observation(
                        &trace_task,
                        AgentLoopTraceEvent::ProviderStarted {
                            request_id,
                            provider_id: contract.provider_id.clone(),
                            model_id: contract.model_id.clone(),
                        },
                    )
                    .await?;
                    let cache_identity = provider_plan
                        .provider
                        .cache_identity_for_request(&provider_request);
                    match tokio::time::timeout(
                        provider_plan.provider_session_policy.request_timeout,
                        provider_plan.provider.complete(provider_request.clone()),
                    )
                    .await
                    {
                        Ok(Ok(response)) => {
                            let usage = auxiliary_provider_usage_record(
                                &provider_request,
                                &response,
                                Some(session_id),
                                budget_snapshot_ref,
                                &contract.cost_model,
                                cache_identity,
                            );
                            self.record_auxiliary_trace_observation(
                                &trace_task,
                                AgentLoopTraceEvent::TokenUsageRecorded(usage),
                            )
                            .await?;
                            let model_summary = response
                                .message
                                .as_ref()
                                .map(|message| message.content.trim().to_owned())
                                .filter(|content| !content.is_empty());
                            self.record_auxiliary_trace_observation(
                                &trace_task,
                                AgentLoopTraceEvent::ProviderCompleted {
                                    request_id,
                                    provider_id: contract.provider_id,
                                    model_id: contract.model_id,
                                    response,
                                },
                            )
                            .await?;
                            if let Some(model_summary) = model_summary {
                                let candidate = compaction_summary_envelope(
                                    &model_summary,
                                    source_range,
                                    source_tokens,
                                    source_checksum,
                                    EXPLICIT_COMPACTION_TOKEN_BUDGET,
                                );
                                if !candidate.is_empty() {
                                    summary = candidate;
                                    strategy = "model_summary";
                                }
                            }
                        }
                        Ok(Err(error)) => {
                            self.record_auxiliary_trace_observation(
                                &trace_task,
                                AgentLoopTraceEvent::ProviderFailed {
                                    request_id,
                                    provider_id: contract.provider_id,
                                    model_id: contract.model_id,
                                    error: error.to_string(),
                                },
                            )
                            .await?;
                        }
                        Err(_) => {
                            self.record_auxiliary_trace_observation(
                                &trace_task,
                                AgentLoopTraceEvent::ProviderFailed {
                                    request_id,
                                    provider_id: contract.provider_id,
                                    model_id: contract.model_id,
                                    error: "compaction summary request timed out".to_owned(),
                                },
                            )
                            .await?;
                        }
                    }
                }
            }
        }
        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            active_task_id,
            RuntimeEventType::CompactionCompleted,
            RuntimeEventSource::Runtime,
            json!({
                "summary": "conversation history compacted",
                "content": summary,
                "command_id": command.command_id,
                "mode": "explicit",
                "strategy": strategy,
            }),
        ))
        .await?;
        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some("conversation history compacted".to_owned()),
        })
    }

    async fn handle_storage_maintenance_command(
        &self,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let report = self.run_storage_maintenance().await?;
        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            None,
            RuntimeEventType::StorageMaintenanceCompleted,
            RuntimeEventSource::Runtime,
            json!({
                "summary": "runtime storage maintenance completed",
                "command_id": command.command_id,
                "report": report,
            }),
        ))
        .await?;
        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some("storage maintenance completed".to_owned()),
        })
    }
}
