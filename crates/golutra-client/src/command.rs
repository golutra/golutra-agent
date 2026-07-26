//! SessionCommand 校验、幂等日志与用例分派。

use super::*;

impl RuntimeHost {
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
        let _command_guard = self.command_mutex.lock().await;
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
            CommandClaim::Existing(ack) => return Ok(ack),
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
                SessionCommandKind::Prompt => {
                    self.clone().handle_prompt(session_id, command).await?
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
                    self.handle_replay_command(session_id, command).await?
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
                _ => {
                    self.record_event(host_event(
                        self.next_sequence_no(),
                        session_id,
                        None,
                        RuntimeEventType::CommandAccepted,
                        RuntimeEventSource::Runtime,
                        json!({
                            "summary": format!("accepted {:?}", command.kind),
                            "command_id": command_id.to_string(),
                            "payload": command.payload,
                        }),
                    ))
                    .await?;
                    CommandAck {
                        command_id,
                        accepted: true,
                        reason: Some(format!("accepted in session {session_id}")),
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
        let explicit_task_contract = payload
            .get("task_contract")
            .is_some_and(|value| !value.is_null());
        let mut task_contract = task_contract_from_payload(&payload)?;
        let contract_origin = if explicit_task_contract {
            "explicit"
        } else {
            provider_runtime::apply_legacy_task_contract(&payload, &prompt, &mut task_contract);
            "legacy_adapter"
        };
        task_contract
            .validate()
            .map_err(ClientError::TaskExecution)?;
        payload["task_contract"] = serde_json::to_value(&task_contract)?;
        payload["_task_contract_origin"] = Value::String(contract_origin.to_owned());
        let busy_decision = {
            let lane_manager = self.lane_manager.lock().await;
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
                self.upsert_current_thread(session_id, &payload).await?;
                let control = self.task_controls.lock().await.get(&session_id).cloned();
                match control {
                    Some(control) if control.task_id == active_task_id => {
                        match control
                            .execution
                            .append_turn(PendingAgentTurn {
                                command_id: command.command_id,
                                turn_id,
                                content: prompt.clone(),
                                task_contract: Some(task_contract.clone()),
                                steer: payload
                                    .get("steer")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false),
                            })
                            .await
                        {
                            Ok(()) => {
                                let transition = self.lane_manager.lock().await.queue_turn(
                                    session_id,
                                    turn_id,
                                    self.next_sequence_no(),
                                )?;
                                self.record_event(with_command_payload(
                                    transition.event,
                                    command.command_id,
                                    payload.clone(),
                                ))
                                .await?;
                            }
                            Err(AgentLoopError::PendingTurnQueueClosed) => {
                                retry_as_new_task = true;
                            }
                            Err(AgentLoopError::PendingTurnQueueFull) => {
                                accepted = false;
                                reason = "active task pending turn queue is full".to_owned();
                            }
                            Err(error) => {
                                return Err(ClientError::TaskExecution(error.to_string()));
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
                        "summary": reason,
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
                        "prompt rejected by runtime lane busy policy".to_owned()
                    }),
                });
            }
        }
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

        self.upsert_current_thread(session_id, &payload).await?;
        let mut lane_manager = self.lane_manager.lock().await;
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
        let requested_network = payload
            .get("allow_network")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        task_created.payload["execution_capabilities"] =
            self.execution_capabilities(requested_network);
        task_created.payload["run_provenance"] =
            serde_json::to_value(self.capture_run_provenance(task_id))?;
        if let Err(error) = self.record_event(task_created).await {
            let _ = self.lane_manager.lock().await.finish_task(
                session_id,
                TaskStatus::Failed,
                self.next_sequence_no(),
            );
            return Err(error);
        }
        self.clone()
            .spawn_agent_task(
                HostedAgentTask {
                    session_id,
                    task_id,
                    turn_id,
                    payload,
                },
                session_lease,
                Vec::new(),
            )
            .await?;

        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some(format!("started task {task_id} in session {session_id}")),
        })
    }

    async fn handle_reconcile_task_command(
        self: &Arc<Self>,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        const MAX_RECONCILIATION_NOTE_CHARS: usize = 4_096;

        let command_id = command.command_id;
        let state = self
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
        let task_control = self.task_controls.lock().await.get(&session_id).cloned();
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
        let mut lane_manager = self.lane_manager.lock().await;
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
        let environment = load_provider_runtime_env_from_paths(&paths)
            .map_err(|error| ClientError::TaskExecution(error.to_string()))?;
        let redacted = ConfiguredProvider::redacted_from_reader(|key| environment.get(key))
            .map_err(|error| ClientError::TaskExecution(error.to_string()))?;
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
                    let event_type = if matches!(error, ProviderError::RateLimited { .. }) {
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

        let requested_id = provider_auth_request_id_from_payload(&command.payload)?;
        let pending = {
            let mut waiters = self.provider_auth_waiters.lock().await;
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
            let mut waiters = self.provider_auth_waiters.lock().await;
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
        let lane = self.lane_manager.lock().await.lane(session_id).cloned();
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
        if !self.task_controls.lock().await.contains_key(&session_id) {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some(
                    "takeover rejected because the session has no locally active task".to_owned(),
                ),
            });
        }
        let transition = self.lane_manager.lock().await.takeover(
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

    async fn handle_approval_command(
        &self,
        session_id: SessionId,
        command: SessionCommand,
        decision: ApprovalDecision,
    ) -> Result<CommandAck, ClientError> {
        if self
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
            .repositories
            .projections
            .state(session_id, None)
            .await?;
        let pending_approval = state
            .pending_approval
            .as_deref()
            .and_then(|value| value.parse::<ApprovalId>().ok());
        let requested_approval = command
            .payload
            .get("approval_id")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<ApprovalId>().ok())
            .or(pending_approval);
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
        let control = self.task_controls.lock().await.get(&session_id).cloned();
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

    async fn handle_compact_command(
        &self,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        if self
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
        let events = self
            .repositories
            .events
            .load_recent(session_id, None, None, MAX_HISTORY_SOURCE_EVENTS)
            .await?;
        let explicit_compaction = self
            .repositories
            .events
            .latest_explicit_compaction(session_id)
            .await?
            .as_ref()
            .and_then(context_compaction_from_event);
        let compacted_after = explicit_compaction
            .as_ref()
            .map(|(sequence_no, _)| *sequence_no)
            .unwrap_or_default();
        let lines = events
            .iter()
            .filter(|event| event.sequence_no > compacted_after)
            .filter(|event| event.event_type.is_model_history_fact())
            .filter_map(conversation_history_line)
            .collect::<Vec<_>>();
        if explicit_compaction.is_none() && lines.is_empty() {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some("session has no conversation history to compact".to_owned()),
            });
        }
        let summary = compact_history_with_summary(
            explicit_compaction.map(|(_, content)| format!("Summary: {content}")),
            lines,
        );
        let active_task_id = self
            .repositories
            .projections
            .state(session_id, None)
            .await?
            .active_task_id;
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
