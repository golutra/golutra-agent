use super::*;

#[derive(Debug, Clone)]
pub(crate) struct RecoverableInput {
    pub(crate) turn_id: TurnId,
    pub(crate) prompt: String,
    pub(crate) steer: bool,
    pub(crate) payload: Value,
}

#[derive(Debug)]
pub(crate) struct PendingRecovery {
    session_id: SessionId,
    task_id: Option<TaskId>,
    inputs: Vec<RecoverableInput>,
    immediate: bool,
    terminal: bool,
    last_sequence_no: u64,
}

impl TuiApp {
    pub(crate) fn defer_rejected_steer(&mut self, prompt: String) {
        let payload = self.runtime_prompt_payload(prompt.clone());
        let recovery = self
            .pending_recovery
            .get_or_insert_with(|| PendingRecovery {
                session_id: self.session_id,
                task_id: None,
                inputs: Vec::new(),
                immediate: true,
                terminal: true,
                last_sequence_no: self
                    .events
                    .iter()
                    .map(|event| event.sequence_no)
                    .max()
                    .unwrap_or(0),
            });
        if recovery.session_id != self.session_id {
            self.status_message =
                "previous session has pending recovery; prompt kept in draft".to_owned();
            return;
        }
        recovery.inputs.push(RecoverableInput {
            turn_id: TurnId::new(),
            prompt,
            steer: true,
            payload,
        });
        self.input.reset();
        self.attachments.clear();
        self.selected_attachment = None;
        self.mention_completion = None;
        self.status_message = "current turn ended; input queued for the next turn".to_owned();
    }

    pub(crate) fn rejected_steer_previews(&self) -> Vec<QueuedPrompt> {
        self.pending_recovery
            .as_ref()
            .filter(|recovery| {
                recovery.session_id == self.session_id
                    && recovery.task_id.is_none()
                    && recovery.terminal
            })
            .map(|recovery| {
                recovery
                    .inputs
                    .iter()
                    .map(|input| QueuedPrompt {
                        turn_id: input.turn_id,
                        prompt: input.prompt.clone(),
                        steer: false,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn capture_pending_recovery(&mut self, immediate: bool) {
        let inputs = queued_prompts(&self.events)
            .into_iter()
            .map(|queued| {
                let mut payload = json!({"prompt": queued.prompt});
                for event in self.events.iter().filter(|event| {
                    event.turn_id == Some(queued.turn_id)
                        && matches!(
                            event.event_type,
                            RuntimeEventType::TurnQueued | RuntimeEventType::TurnUpdated
                        )
                }) {
                    if let Some(update) = event
                        .payload
                        .get("payload")
                        .unwrap_or(&event.payload)
                        .as_object()
                    {
                        for (key, value) in update {
                            payload[key] = value.clone();
                        }
                    }
                }
                RecoverableInput {
                    turn_id: queued.turn_id,
                    prompt: queued.prompt,
                    steer: queued.steer,
                    payload,
                }
            })
            .collect::<Vec<_>>();
        self.pending_recovery = Some(PendingRecovery {
            session_id: self.session_id,
            task_id: self
                .projection
                .as_ref()
                .and_then(|projection| projection.task_id),
            immediate,
            inputs,
            terminal: false,
            last_sequence_no: self
                .events
                .iter()
                .map(|event| event.sequence_no)
                .max()
                .unwrap_or(0),
        });
    }

    pub(crate) fn update_pending_recovery(&mut self, event: &RuntimeEvent) {
        let Some(recovery) = &mut self.pending_recovery else {
            return;
        };
        if recovery.session_id != event.session_id || event.sequence_no <= recovery.last_sequence_no
        {
            return;
        }
        recovery.last_sequence_no = event.sequence_no;
        match event.event_type {
            RuntimeEventType::TurnQueued | RuntimeEventType::TurnUpdated
                if !recovery.terminal
                    && recovery.task_id.is_none_or(|id| Some(id) == event.task_id) =>
            {
                let Some(turn_id) = event.turn_id else {
                    return;
                };
                let payload = event.payload.get("payload").unwrap_or(&event.payload);
                let Some(prompt) = payload.get("prompt").and_then(Value::as_str) else {
                    return;
                };
                if let Some(input) = recovery
                    .inputs
                    .iter_mut()
                    .find(|input| input.turn_id == turn_id)
                {
                    input.prompt = prompt.to_owned();
                    if let Some(steer) = payload.get("steer").and_then(Value::as_bool) {
                        input.steer = steer;
                    }
                    if let Some(update) = payload.as_object() {
                        for (key, value) in update {
                            input.payload[key] = value.clone();
                        }
                    }
                } else {
                    recovery.inputs.push(RecoverableInput {
                        turn_id,
                        prompt: prompt.to_owned(),
                        payload: payload.clone(),
                        steer: payload
                            .get("steer")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    });
                }
            }
            RuntimeEventType::TurnStarted | RuntimeEventType::TurnCancelled => {
                recovery
                    .inputs
                    .retain(|input| Some(input.turn_id) != event.turn_id);
            }
            kind if !recovery.terminal
                && kind.is_task_terminal()
                && recovery.task_id.is_none_or(|id| Some(id) == event.task_id) =>
            {
                recovery.terminal = true;
                recovery.immediate &= matches!(
                    kind,
                    RuntimeEventType::TaskAborted | RuntimeEventType::TaskInterrupted
                );
            }
            _ => {}
        }
    }

    pub(crate) fn restore_pending_inputs(&mut self, inputs: &[RecoverableInput]) {
        if inputs.is_empty() {
            return;
        }
        let mut parts = inputs
            .iter()
            .map(|input| input.prompt.clone())
            .collect::<Vec<_>>();
        if !self.input.is_empty() {
            parts.push(self.input.text().to_owned());
        }
        self.input.set_text(parts.join("\n"));
        for input in inputs {
            for value in input
                .payload
                .get("attachments")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(path) = value.get("path").and_then(Value::as_str) else {
                    continue;
                };
                if self
                    .attachments
                    .iter()
                    .any(|attachment| attachment.display_path == path)
                {
                    continue;
                }
                let kind = match value.get("kind").and_then(Value::as_str) {
                    Some("image") => AttachmentKind::Image,
                    Some("text") => AttachmentKind::Text,
                    _ => AttachmentKind::Binary,
                };
                self.attachments.push(ComposerAttachment {
                    path: self.workspace_path.join(path),
                    display_path: path.to_owned(),
                    kind,
                    bytes: value.get("bytes").and_then(Value::as_u64).unwrap_or(0),
                });
            }
        }
        self.editing_queued_turn = None;
        self.prompt_history.reset_navigation();
        self.status_message = "pending messages restored to draft".to_owned();
    }

    pub(crate) async fn poll_pending_recovery(
        &mut self,
        transport: &RuntimeTransport,
    ) -> miette::Result<bool> {
        if let Some(recovery) = &self.pending_recovery
            && recovery.session_id == self.session_id
        {
            let mut unseen = self
                .events
                .iter()
                .filter(|event| {
                    event.sequence_no > recovery.last_sequence_no
                        && (event.event_type.is_task_terminal()
                            || matches!(
                                event.event_type,
                                RuntimeEventType::TurnQueued
                                    | RuntimeEventType::TurnUpdated
                                    | RuntimeEventType::TurnStarted
                                    | RuntimeEventType::TurnCancelled
                            ))
                })
                .cloned()
                .collect::<Vec<_>>();
            unseen.sort_by_key(|event| event.sequence_no);
            for event in unseen {
                self.update_pending_recovery(&event);
            }
        }
        if has_active_task(self)
            || self.overlay_surface().is_some()
            || self.auth_operation.is_some()
            || !self
                .pending_recovery
                .as_ref()
                .is_some_and(|recovery| recovery.terminal && recovery.session_id == self.session_id)
        {
            return Ok(false);
        }
        let recovery = self.pending_recovery.take().expect("ready recovery");
        if !recovery.immediate || !recovery.inputs.iter().any(|input| input.steer) {
            self.restore_pending_inputs(&recovery.inputs);
            return Ok(true);
        }
        let (steers, followups): (Vec<_>, Vec<_>) =
            recovery.inputs.into_iter().partition(|input| input.steer);
        let prompt = steers
            .iter()
            .map(|input| input.prompt.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let mut payload = self.runtime_prompt_payload(prompt.clone());
        let attachments = steers
            .iter()
            .flat_map(|input| {
                input
                    .payload
                    .get("attachments")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .cloned()
            })
            .collect::<Vec<_>>();
        payload["attachments"] = json!(attachments);
        let mut batches = vec![(prompt, payload, steers)];
        batches.extend(
            followups
                .into_iter()
                .map(|input| (input.prompt.clone(), input.payload.clone(), vec![input])),
        );
        let mut batches = batches.into_iter();
        while let Some((prompt, payload, originals)) = batches.next() {
            let result = transport
                .send_command(session_command(
                    self.session_id,
                    SessionCommandKind::Prompt,
                    payload,
                ))
                .await;
            match result {
                Ok(ack) if ack.accepted => {
                    self.prompt_history.record(&prompt);
                    if self.last_prompt_ack.as_ref().is_some_and(|previous| {
                        !previous.accepted
                            && previous.reason.as_deref()
                                == Some("steering requires an active runtime task")
                    }) {
                        self.last_prompt_ack = Some(ack);
                    }
                }
                result => {
                    let reason = match result {
                        Ok(ack) => compact_ack_reason(&ack.reason),
                        Err(error) => error.to_string(),
                    };
                    let mut unsent = originals;
                    unsent.extend(batches.flat_map(|(_, _, originals)| originals));
                    self.restore_pending_inputs(&unsent);
                    self.push_system_message(
                        "Pending input",
                        vec![format!(
                            "{reason}; messages kept in draft, review before resending"
                        )],
                    );
                    break;
                }
            }
        }
        self.refresh(transport).await?;
        Ok(true)
    }
}
