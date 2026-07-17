//! Memory、post-task、regression 与 promotion 治理命令。

use super::*;

impl RuntimeHost {
    pub(super) async fn handle_memory_rollback_command(
        &self,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let memory_id = command
            .payload
            .get("memory_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ClientError::InvalidSession("memory_id is required".to_owned()))?
            .parse::<MemoryId>()
            .map_err(|error| ClientError::InvalidSession(error.to_string()))?;
        let reason = command
            .payload
            .get("reason")
            .and_then(Value::as_str)
            .filter(|reason| !reason.trim().is_empty())
            .unwrap_or("rolled back by user")
            .to_owned();
        let memory_store = self.memory_store.clone();
        let record = run_blocking(move || memory_store.rollback(memory_id, reason)).await??;
        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            None,
            RuntimeEventType::MemoryRolledBack,
            RuntimeEventSource::Memory,
            json!({
                "summary": format!("project memory {memory_id} rolled back"),
                "record": record,
                "command_id": command.command_id,
            }),
        ))
        .await?;
        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some(format!("project memory {memory_id} rolled back")),
        })
    }

    pub(super) async fn handle_memory_feedback_command(
        &self,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let memory_id = command
            .payload
            .get("memory_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ClientError::InvalidSession("memory_id is required".to_owned()))?
            .parse::<MemoryId>()
            .map_err(|error| ClientError::InvalidSession(error.to_string()))?;
        let feedback = match command.payload.get("feedback").and_then(Value::as_str) {
            Some("helpful") => MemoryFeedbackKind::Helpful,
            Some("irrelevant") => MemoryFeedbackKind::Irrelevant,
            Some("incorrect") => MemoryFeedbackKind::Incorrect,
            _ => {
                return Ok(CommandAck {
                    command_id: command.command_id,
                    accepted: false,
                    reason: Some(
                        "memory feedback must be helpful, irrelevant, or incorrect".to_owned(),
                    ),
                });
            }
        };
        let reason = command
            .payload
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let memory_store = self.memory_store.clone();
        let record =
            run_blocking(move || memory_store.record_feedback(memory_id, feedback, reason))
                .await??;
        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            None,
            RuntimeEventType::MemoryFeedbackRecorded,
            RuntimeEventSource::Memory,
            json!({
                "summary": format!("project memory {memory_id} feedback recorded"),
                "feedback": feedback,
                "record": record,
                "command_id": command.command_id,
            }),
        ))
        .await?;
        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some(format!("project memory {memory_id} feedback recorded")),
        })
    }

    pub(super) async fn handle_review_memory_candidate_command(
        &self,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let memory_id = command
            .payload
            .get("memory_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ClientError::InvalidSession("memory_id is required".to_owned()))?
            .parse::<MemoryId>()
            .map_err(|error| ClientError::InvalidSession(error.to_string()))?;
        let decision = command
            .payload
            .get("decision")
            .and_then(Value::as_str)
            .unwrap_or("approve");
        let memory_store = self.memory_store.clone();
        if decision == "reject" {
            let record =
                run_blocking(move || memory_store.rollback(memory_id, "human review rejected"))
                    .await??;
            self.record_event(host_event(
                self.next_sequence_no(),
                session_id,
                None,
                RuntimeEventType::MemoryInvalidated,
                RuntimeEventSource::Memory,
                json!({
                    "summary": format!("memory {memory_id} rejected during review"),
                    "record": record,
                    "command_id": command.command_id,
                }),
            ))
            .await?;
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: true,
                reason: Some(format!("memory {memory_id} rejected")),
            });
        }
        if decision != "approve" {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some("memory review decision must be approve or reject".to_owned()),
            });
        }
        let supporting_task_ids = command
            .payload
            .get("supporting_task_ids")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .filter_map(|value| value.parse::<TaskId>().ok())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let reviewer = command.actor.id.clone();
        let record = run_blocking(move || {
            memory_store.activate_quarantined(memory_id, &supporting_task_ids, Some(&reviewer))
        })
        .await??;
        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            None,
            RuntimeEventType::MemoryActivated,
            RuntimeEventSource::Memory,
            json!({
                "summary": format!("memory {memory_id} activated after review"),
                "record": record,
                "command_id": command.command_id,
            }),
        ))
        .await?;
        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some(format!("memory {memory_id} activated")),
        })
    }

    pub(super) async fn handle_expire_memory_command(
        &self,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let memory_id = command
            .payload
            .get("memory_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ClientError::InvalidSession("memory_id is required".to_owned()))?
            .parse::<MemoryId>()
            .map_err(|error| ClientError::InvalidSession(error.to_string()))?;
        let memory_store = self.memory_store.clone();
        let record = run_blocking(move || memory_store.expire(memory_id)).await??;
        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            None,
            RuntimeEventType::MemoryInvalidated,
            RuntimeEventSource::Memory,
            json!({
                "summary": format!("memory {memory_id} expired"),
                "record": record,
                "command_id": command.command_id,
            }),
        ))
        .await?;
        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some(format!("memory {memory_id} expired")),
        })
    }

    pub(super) async fn handle_regression_command(
        &self,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let candidate_id = candidate_id_from_payload(&command.payload)?.to_owned();
        let source_task_id = self
            .governance
            .candidate_source_task_id(&candidate_id)
            .await?;
        self.ensure_task_in_session(session_id, source_task_id)
            .await?;
        self.wait_for_candidate_evaluation(&candidate_id).await;
        let regression = self
            .run_regression_campaign(session_id, &command, &candidate_id)
            .await?;
        let evaluation_store = self.evaluation_store.clone();
        let decision = run_blocking({
            let candidate_id = candidate_id.clone();
            move || evaluation_store.decide_after_regression(&candidate_id)
        })
        .await??;
        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            Some(source_task_id),
            RuntimeEventType::RegressionCompleted,
            RuntimeEventSource::Evaluator,
            json!({
                "summary": format!("candidate {candidate_id} regression completed"),
                "record": regression,
                "command_id": command.command_id,
            }),
        ))
        .await?;
        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            Some(source_task_id),
            RuntimeEventType::PromotionDecided,
            RuntimeEventSource::Evaluator,
            json!({
                "summary": format!(
                    "candidate {candidate_id} post-regression decision: {:?}",
                    decision.decision
                ),
                "record": decision,
                "command_id": command.command_id,
            }),
        ))
        .await?;
        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some(format!("candidate {candidate_id} regression completed")),
        })
    }

    pub(super) async fn handle_wait_post_task_job_command(
        &self,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let task_id = command
            .payload
            .get("task_id")
            .and_then(Value::as_str)
            .map(|value| {
                value.parse().map_err(|error: uuid::Error| {
                    ClientError::InvalidSession(format!("task_id is invalid: {error}"))
                })
            })
            .transpose()?;
        let task_id = match task_id {
            Some(task_id) => task_id,
            None => {
                let job_id = command
                    .payload
                    .get("job_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        ClientError::InvalidSession("task_id or job_id is required".to_owned())
                    })?
                    .parse::<PostTaskJobId>()
                    .map_err(|error| ClientError::InvalidSession(error.to_string()))?;
                let job = self.repositories.jobs.by_id(job_id).await?.ok_or_else(|| {
                    ClientError::InvalidSession(format!("post-task job `{job_id}` not found"))
                })?;
                post_task::ensure_job_in_workspace(self, Some(&job), job.task_id)?;
                job.task_id
            }
        };
        self.ensure_task_in_session(session_id, task_id).await?;
        self.wait_for_deep_task_evaluation(task_id).await;
        let job = self.repositories.jobs.get_for_task(task_id).await?;
        post_task::ensure_job_in_workspace(self, job.as_ref(), task_id)?;
        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            Some(task_id),
            RuntimeEventType::CommandAccepted,
            RuntimeEventSource::Evaluator,
            json!({
                "summary": "waited for durable post-task evaluation",
                "task_id": task_id,
                "job": job,
                "command_id": command.command_id,
            }),
        ))
        .await?;
        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some(format!("post-task evaluation settled for task {task_id}")),
        })
    }

    pub(super) async fn handle_retry_post_task_job_command(
        &self,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let job_id = command
            .payload
            .get("job_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ClientError::InvalidSession("job_id is required".to_owned()))?
            .parse::<PostTaskJobId>()
            .map_err(|error| ClientError::InvalidSession(error.to_string()))?;
        let job = self.repositories.jobs.by_id(job_id).await?.ok_or_else(|| {
            ClientError::InvalidSession(format!("post-task job `{job_id}` not found"))
        })?;
        post_task::ensure_job_in_workspace(self, Some(&job), job.task_id)?;
        self.ensure_task_in_session(session_id, job.task_id).await?;
        let retried = self.repositories.jobs.retry(job_id).await?;
        if retried {
            self.record_event(host_event(
                self.next_sequence_no(),
                session_id,
                None,
                RuntimeEventType::RetryScheduled,
                RuntimeEventSource::Evaluator,
                json!({
                    "summary": "post-task evaluation manually retried",
                    "job_id": job_id,
                    "command_id": command.command_id,
                }),
            ))
            .await?;
        }
        Ok(CommandAck {
            command_id: command.command_id,
            accepted: retried,
            reason: Some(if retried {
                format!("post-task job {job_id} requeued")
            } else {
                format!("post-task job {job_id} is not retryable")
            }),
        })
    }

    pub(super) async fn handle_review_candidate_command(
        &self,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let candidate_id = candidate_id_from_payload(&command.payload)?.to_owned();
        let source_task_id = self
            .governance
            .candidate_source_task_id(&candidate_id)
            .await?;
        self.ensure_task_in_session(session_id, source_task_id)
            .await?;
        self.wait_for_candidate_evaluation(&candidate_id).await;
        let decision = match command.payload.get("decision").and_then(Value::as_str) {
            Some("approve") => PromotionDecisionKind::Approve,
            Some("reject") => PromotionDecisionKind::Reject,
            _ => {
                return Ok(CommandAck {
                    command_id: command.command_id,
                    accepted: false,
                    reason: Some("candidate review decision must be approve or reject".to_owned()),
                });
            }
        };
        let reason = command
            .payload
            .get("reason")
            .and_then(Value::as_str)
            .filter(|reason| !reason.trim().is_empty())
            .unwrap_or("reviewed by runtime controller")
            .to_owned();
        let reviewer_id = command.actor.id.clone();
        let evaluation_store = self.evaluation_store.clone();
        let review = run_blocking({
            let candidate_id = candidate_id.clone();
            move || {
                evaluation_store.review_promotion(&candidate_id, decision, &reviewer_id, &reason)
            }
        })
        .await??;
        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            Some(source_task_id),
            RuntimeEventType::PromotionDecided,
            RuntimeEventSource::Evaluator,
            json!({
                "summary": format!("candidate {candidate_id} reviewed as {decision:?}"),
                "record": review,
                "command_id": command.command_id,
            }),
        ))
        .await?;
        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some(format!("candidate {candidate_id} reviewed as {decision:?}")),
        })
    }

    pub(super) async fn handle_record_benchmark_command(
        &self,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let run: BenchmarkRun =
            serde_json::from_value(command.payload.get("run").cloned().ok_or_else(|| {
                ClientError::InvalidSession("benchmark run is required".to_owned())
            })?)?;
        let benchmark_id = run.benchmark_id.clone();
        let evaluation_store = self.evaluation_store.clone();
        run_blocking(move || evaluation_store.record_benchmark_run(run)).await??;
        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            None,
            RuntimeEventType::BenchmarkRecorded,
            RuntimeEventSource::Evaluator,
            json!({
                "summary": format!("benchmark run {benchmark_id} recorded"),
                "benchmark_id": benchmark_id,
                "command_id": command.command_id,
            }),
        ))
        .await?;
        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some(format!("benchmark run {benchmark_id} recorded")),
        })
    }

    pub(super) async fn handle_compare_counterfactual_command(
        &self,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let group_id = command
            .payload
            .get("group_id")
            .and_then(Value::as_str)
            .filter(|group_id| !group_id.trim().is_empty())
            .ok_or_else(|| {
                ClientError::InvalidSession("counterfactual group_id is required".to_owned())
            })?
            .to_owned();
        let evaluation_store = self.evaluation_store.clone();
        let comparison = run_blocking({
            let group_id = group_id.clone();
            move || evaluation_store.compare_counterfactual(&group_id)
        })
        .await??;
        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            None,
            RuntimeEventType::CounterfactualCompared,
            RuntimeEventSource::Evaluator,
            json!({
                "summary": format!("counterfactual group {group_id} compared"),
                "record": comparison,
                "command_id": command.command_id,
            }),
        ))
        .await?;
        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some(format!("counterfactual group {group_id} compared")),
        })
    }

    pub(super) async fn handle_apply_candidate_command(
        &self,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let candidate_id = candidate_id_from_payload(&command.payload)?.to_owned();
        let source_task_id = self
            .governance
            .candidate_source_task_id(&candidate_id)
            .await?;
        self.ensure_task_in_session(session_id, source_task_id)
            .await?;
        self.wait_for_candidate_evaluation(&candidate_id).await;
        let evaluation_store = self.evaluation_store.clone();
        let candidate_status = run_blocking({
            let candidate_id = candidate_id.clone();
            move || {
                evaluation_store
                    .snapshot()?
                    .automation_candidates
                    .into_iter()
                    .find(|candidate| candidate.id == candidate_id)
                    .map(|candidate| candidate.status)
                    .ok_or(EvaluationError::CandidateNotFound(candidate_id))
            }
        })
        .await??;
        if candidate_status == CandidateStatus::RegressionPassed {
            let evaluation_store = self.evaluation_store.clone();
            let decision = run_blocking({
                let candidate_id = candidate_id.clone();
                move || evaluation_store.decide_promotion(&candidate_id)
            })
            .await??;
            let approved = decision.decision == PromotionDecisionKind::Approve;
            self.record_event(host_event(
                self.next_sequence_no(),
                session_id,
                Some(source_task_id),
                RuntimeEventType::PromotionDecided,
                RuntimeEventSource::Evaluator,
                json!({
                    "summary": format!("candidate {candidate_id} promotion decision: {:?}", decision.decision),
                    "record": decision,
                    "command_id": command.command_id,
                }),
            ))
            .await?;
            if !approved {
                return Ok(CommandAck {
                    command_id: command.command_id,
                    accepted: false,
                    reason: Some(format!(
                        "candidate {candidate_id} requires explicit human review"
                    )),
                });
            }
        } else if candidate_status == CandidateStatus::NeedsHumanReview {
            return Ok(CommandAck {
                command_id: command.command_id,
                accepted: false,
                reason: Some(format!(
                    "candidate {candidate_id} requires explicit human review before apply"
                )),
            });
        }
        let evaluation_store = self.evaluation_store.clone();
        let applied = run_blocking({
            let candidate_id = candidate_id.clone();
            move || evaluation_store.apply_candidate(&candidate_id)
        })
        .await??;
        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            Some(source_task_id),
            RuntimeEventType::CandidateApplied,
            RuntimeEventSource::Evaluator,
            json!({
                "summary": format!("candidate {candidate_id} applied"),
                "record": applied,
                "command_id": command.command_id,
            }),
        ))
        .await?;
        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some(format!("candidate {candidate_id} applied")),
        })
    }

    pub(super) async fn handle_rollback_candidate_command(
        &self,
        session_id: SessionId,
        command: SessionCommand,
    ) -> Result<CommandAck, ClientError> {
        let candidate_id = candidate_id_from_payload(&command.payload)?.to_owned();
        let source_task_id = self
            .governance
            .candidate_source_task_id(&candidate_id)
            .await?;
        self.ensure_task_in_session(session_id, source_task_id)
            .await?;
        let reason = command
            .payload
            .get("reason")
            .and_then(Value::as_str)
            .filter(|reason| !reason.trim().is_empty())
            .unwrap_or("rolled back by user")
            .to_owned();
        let evaluation_store = self.evaluation_store.clone();
        let rolled_back = run_blocking({
            let candidate_id = candidate_id.clone();
            move || evaluation_store.rollback_candidate(&candidate_id, reason)
        })
        .await??;
        self.record_event(host_event(
            self.next_sequence_no(),
            session_id,
            Some(source_task_id),
            RuntimeEventType::CandidateRolledBack,
            RuntimeEventSource::Evaluator,
            json!({
                "summary": format!("candidate {candidate_id} rolled back"),
                "record": rolled_back,
                "command_id": command.command_id,
            }),
        ))
        .await?;
        Ok(CommandAck {
            command_id: command.command_id,
            accepted: true,
            reason: Some(format!("candidate {candidate_id} rolled back")),
        })
    }
}
