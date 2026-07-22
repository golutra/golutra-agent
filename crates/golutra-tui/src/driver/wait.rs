use std::collections::{HashMap, HashSet};

use golutra_core::{TaskId, TaskStatus, TurnId};
use golutra_protocol::{RuntimeEvent, RuntimeEventType, WaitCondition};

use super::session::{event_type_name, is_terminal_status, runtime_events};
use super::{SubmissionAnchor, TuiApp};

#[derive(Debug, Clone, Copy)]
struct AnchorEvent {
    sequence_no: u64,
    task_id: Option<TaskId>,
    turn_id: Option<TurnId>,
}

#[derive(Debug, Default)]
struct ScopedSequences {
    by_task: HashMap<TaskId, u64>,
    by_turn: HashMap<(TaskId, TurnId), u64>,
    without_turn: HashMap<TaskId, u64>,
}

impl ScopedSequences {
    fn record(&mut self, event: &RuntimeEvent) {
        let Some(task_id) = event.task_id else {
            return;
        };
        record_max(&mut self.by_task, task_id, event.sequence_no);
        if let Some(turn_id) = event.turn_id {
            record_max(&mut self.by_turn, (task_id, turn_id), event.sequence_no);
        } else {
            record_max(&mut self.without_turn, task_id, event.sequence_no);
        }
    }

    fn matches(&self, anchor: SubmissionAnchor) -> bool {
        let Some(task_id) = anchor.task_id else {
            return false;
        };
        self.matches_scope(task_id, anchor.turn_id, anchor.after_sequence_no)
    }

    fn matches_scope(
        &self,
        task_id: TaskId,
        turn_id: Option<TurnId>,
        after_sequence_no: Option<u64>,
    ) -> bool {
        let sequence_no = match turn_id {
            Some(turn_id) => self
                .by_turn
                .get(&(task_id, turn_id))
                .into_iter()
                .chain(self.without_turn.get(&task_id))
                .copied()
                .max(),
            None => self.by_task.get(&task_id).copied(),
        };
        sequence_no.is_some_and(|sequence_no| {
            after_sequence_no.is_none_or(|after_sequence_no| sequence_no > after_sequence_no)
        })
    }
}

#[derive(Debug, Default)]
struct EvaluationJobs {
    queued: HashSet<String>,
    terminal: HashSet<String>,
}

/// Immutable event index shared by every wait evaluated in one protocol tick.
pub(super) struct WaitFacts {
    projection_ready: bool,
    status: Option<TaskStatus>,
    current_task_id: Option<TaskId>,
    current_turn_id: Option<TurnId>,
    command_anchors: HashMap<String, AnchorEvent>,
    task_terminal: HashMap<TaskId, u64>,
    turn_terminal: ScopedSequences,
    approval_required: ScopedSequences,
    authentication_required: ScopedSequences,
    event_high_watermarks: HashMap<String, u64>,
    evaluation_jobs: HashMap<TaskId, EvaluationJobs>,
}

impl WaitFacts {
    pub(super) fn from_app(app: &TuiApp) -> Self {
        let events = runtime_events(app);
        let current_task_id = app
            .task_id
            .or_else(|| {
                app.projection
                    .as_ref()
                    .and_then(|projection| projection.task_id)
            })
            .or_else(|| events.iter().rev().find_map(|event| event.task_id));
        let current_turn_id = current_task_id
            .and_then(|task_id| {
                events
                    .iter()
                    .rev()
                    .find(|event| event.task_id == Some(task_id))
                    .and_then(|event| event.turn_id)
            })
            .or_else(|| events.iter().rev().find_map(|event| event.turn_id));
        let mut facts = Self {
            projection_ready: app.projection.is_some(),
            status: app.projection.as_ref().map(|projection| projection.status),
            current_task_id,
            current_turn_id,
            command_anchors: HashMap::new(),
            task_terminal: HashMap::new(),
            turn_terminal: ScopedSequences::default(),
            approval_required: ScopedSequences::default(),
            authentication_required: ScopedSequences::default(),
            event_high_watermarks: HashMap::new(),
            evaluation_jobs: HashMap::new(),
        };
        for event in events {
            facts.record(event);
        }
        facts
    }

    fn record(&mut self, event: &RuntimeEvent) {
        record_max(
            &mut self.event_high_watermarks,
            event_type_name(event.event_type),
            event.sequence_no,
        );
        if matches!(
            event.event_type,
            RuntimeEventType::TaskCreated | RuntimeEventType::TurnQueued
        ) && let Some(command_id) = event
            .payload
            .get("command_id")
            .and_then(serde_json::Value::as_str)
        {
            self.command_anchors
                .entry(command_id.to_owned())
                .or_insert(AnchorEvent {
                    sequence_no: event.sequence_no,
                    task_id: event.task_id,
                    turn_id: event.turn_id,
                });
        }
        match event.event_type {
            event_type if event_type.is_task_terminal() => {
                if let Some(task_id) = event.task_id {
                    record_max(&mut self.task_terminal, task_id, event.sequence_no);
                }
                self.turn_terminal.record(event);
            }
            RuntimeEventType::AssistantMessage => self.turn_terminal.record(event),
            RuntimeEventType::ApprovalRequested => self.approval_required.record(event),
            RuntimeEventType::ProviderAuthRequired => self.authentication_required.record(event),
            RuntimeEventType::PostTaskJobQueued => {
                if let (Some(task_id), Some(job_id)) = (
                    event.task_id,
                    event
                        .payload
                        .get("job")
                        .and_then(|job| job.get("job_id"))
                        .and_then(serde_json::Value::as_str),
                ) {
                    self.evaluation_jobs
                        .entry(task_id)
                        .or_default()
                        .queued
                        .insert(job_id.to_owned());
                }
            }
            RuntimeEventType::PostTaskJobCompleted | RuntimeEventType::PostTaskJobFailed => {
                if let (Some(task_id), Some(job_id)) = (
                    event.task_id,
                    event
                        .payload
                        .get("job_id")
                        .and_then(serde_json::Value::as_str),
                ) {
                    self.evaluation_jobs
                        .entry(task_id)
                        .or_default()
                        .terminal
                        .insert(job_id.to_owned());
                }
            }
            _ => {}
        }
    }

    pub(super) fn resolve_anchor(&self, mut anchor: SubmissionAnchor) -> SubmissionAnchor {
        if anchor.task_id.is_some() && anchor.turn_id.is_some() {
            return anchor;
        }
        if let Some(event) = self.command_anchors.get(&anchor.command_id.to_string())
            && sequence_after_anchor(event.sequence_no, anchor)
        {
            anchor.task_id = event.task_id;
            anchor.turn_id = event.turn_id;
        }
        anchor
    }

    pub(super) fn condition_met(
        &self,
        condition: &WaitCondition,
        submission: Option<SubmissionAnchor>,
    ) -> bool {
        let submission = submission.map(|anchor| self.resolve_anchor(anchor));
        match condition {
            WaitCondition::Ready => self.projection_ready,
            WaitCondition::Idle => self
                .status
                .is_some_and(|status| status == TaskStatus::Idle || is_terminal_status(status)),
            WaitCondition::TaskStarted => submission.map_or_else(
                || self.status.is_some_and(|status| status != TaskStatus::Idle),
                |anchor| anchor.task_id.is_some(),
            ),
            WaitCondition::TaskTerminal => submission.map_or_else(
                || self.status.is_some_and(is_terminal_status),
                |anchor| {
                    anchor.task_id.is_some_and(|task_id| {
                        self.task_terminal
                            .get(&task_id)
                            .is_some_and(|sequence_no| sequence_after_anchor(*sequence_no, anchor))
                    })
                },
            ),
            WaitCondition::TurnTerminal => submission.map_or_else(
                || {
                    self.current_task_id.zip(self.current_turn_id).is_some_and(
                        |(task_id, turn_id)| {
                            self.turn_terminal
                                .matches_scope(task_id, Some(turn_id), None)
                        },
                    )
                },
                |anchor| self.turn_terminal.matches(anchor),
            ),
            WaitCondition::ApprovalRequired => submission
                .map_or(self.status == Some(TaskStatus::WaitingApproval), |anchor| {
                    self.approval_required.matches(anchor)
                }),
            WaitCondition::AuthenticationRequired => submission.map_or(
                self.status == Some(TaskStatus::WaitingAuthentication),
                |anchor| self.authentication_required.matches(anchor),
            ),
            WaitCondition::EvaluationTerminal => {
                let task_id = match submission {
                    Some(anchor) => anchor.task_id,
                    None => self.current_task_id,
                };
                task_id.is_some_and(|task_id| self.evaluation_terminal(task_id))
            }
            WaitCondition::Event {
                event_type,
                sequence_at_least,
            } => self
                .event_high_watermarks
                .get(&event_type.to_ascii_lowercase())
                .is_some_and(|sequence_no| {
                    sequence_at_least.is_none_or(|minimum| *sequence_no >= minimum)
                        && submission
                            .is_none_or(|anchor| sequence_after_anchor(*sequence_no, anchor))
                }),
        }
    }

    pub(super) fn evaluation_terminal(&self, task_id: TaskId) -> bool {
        self.evaluation_jobs
            .get(&task_id)
            .is_some_and(|jobs| !jobs.queued.is_empty() && jobs.queued.is_subset(&jobs.terminal))
    }
}

fn sequence_after_anchor(sequence_no: u64, anchor: SubmissionAnchor) -> bool {
    anchor
        .after_sequence_no
        .is_none_or(|after_sequence_no| sequence_no > after_sequence_no)
}

fn record_max<K>(values: &mut HashMap<K, u64>, key: K, sequence_no: u64)
where
    K: Eq + std::hash::Hash,
{
    values
        .entry(key)
        .and_modify(|current| *current = (*current).max(sequence_no))
        .or_insert(sequence_no);
}
