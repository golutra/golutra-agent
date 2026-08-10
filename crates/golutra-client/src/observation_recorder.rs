//! Bounded ingress for AgentLoop observations.
//!
//! Provider stream deltas are high-frequency live signals that can be
//! coalesced. Terminal states, tool results, and checkpoints are durable facts
//! that must be retained. This module owns ingress ordering, capacity, and
//! coalescing semantics; it does not convert observations into RuntimeEvents.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use golutra_core::{ProviderRequestId, ToolCallId, ToolProgress};
use golutra_llm::ProviderStreamEvent;
use golutra_runtime::RuntimeObservation;
use tokio::sync::{Notify, oneshot};

/// The synchronous observation seam cannot await a bounded durable queue
/// without risking a runtime-thread deadlock. Both lanes therefore have hard
/// count and byte limits; overload is reported to the owning task so a durable
/// fact is never silently discarded.
const MAX_PENDING_LIVE_OBSERVATIONS: usize = 512;
const MAX_PENDING_COMMANDS: usize = 4_096;
const MAX_PENDING_COMMAND_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CoalescingSummary {
    /// Number of deltas replaced by this observation instead of persisted.
    pub(crate) omitted_events: u32,
    /// UTF-8 bytes or tool-call identifier bytes represented by omitted deltas.
    pub(crate) omitted_bytes: u64,
}

#[derive(Debug)]
pub(crate) enum ObservationCommand {
    Event {
        observation: Box<RuntimeObservation>,
        coalescing: CoalescingSummary,
    },
    Flush(oneshot::Sender<Result<(), ObservationSendError>>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObservationSendError {
    Closed,
    Overloaded,
}

impl std::fmt::Display for ObservationSendError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Closed => "observation recorder is closed",
            Self::Overloaded => "observation recorder queue is overloaded",
        })
    }
}

impl std::error::Error for ObservationSendError {}

#[derive(Debug)]
pub(crate) struct ObservationSender {
    queue: Arc<ObservationQueue>,
}

#[derive(Debug)]
pub(crate) struct ObservationReceiver {
    queue: Arc<ObservationQueue>,
}

#[derive(Debug)]
struct ObservationQueue {
    state: Mutex<QueueState>,
    notify: Notify,
}

#[derive(Debug)]
struct QueueState {
    /// Required/supporting observations and flushed live observations. This
    /// lane is lossless up to the hard queue limits; beyond them the sender
    /// receives `Overloaded` and the owning task is cancelled.
    commands: VecDeque<ObservationCommand>,
    pending_live: VecDeque<PendingLive>,
    command_bytes: usize,
    pending_live_bytes: usize,
    limits: QueueLimits,
    sender_count: usize,
    closed: bool,
}

#[derive(Debug, Clone, Copy)]
struct QueueLimits {
    max_commands: usize,
    max_bytes: usize,
    max_pending_live: usize,
}

#[derive(Debug)]
struct PendingLive {
    key: LiveKey,
    observation: Box<RuntimeObservation>,
    coalescing: CoalescingSummary,
    bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LiveKey {
    ProviderStream(StreamKey),
    ToolProgress(ToolCallId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StreamKey {
    request_id: ProviderRequestId,
    provider_id: String,
    model_id: String,
}

pub(crate) fn channel() -> (ObservationSender, ObservationReceiver) {
    channel_with_limits(QueueLimits {
        max_commands: MAX_PENDING_COMMANDS,
        max_bytes: MAX_PENDING_COMMAND_BYTES,
        max_pending_live: MAX_PENDING_LIVE_OBSERVATIONS,
    })
}

fn channel_with_limits(limits: QueueLimits) -> (ObservationSender, ObservationReceiver) {
    let queue = Arc::new(ObservationQueue {
        state: Mutex::new(QueueState {
            commands: VecDeque::new(),
            pending_live: VecDeque::with_capacity(limits.max_pending_live),
            command_bytes: 0,
            pending_live_bytes: 0,
            limits,
            sender_count: 1,
            closed: false,
        }),
        notify: Notify::new(),
    });
    (
        ObservationSender {
            queue: queue.clone(),
        },
        ObservationReceiver { queue },
    )
}

impl Clone for ObservationSender {
    fn clone(&self) -> Self {
        let mut state = self.lock_state();
        state.sender_count = state
            .sender_count
            .checked_add(1)
            .expect("observation sender count overflow");
        drop(state);
        Self {
            queue: self.queue.clone(),
        }
    }
}

impl ObservationSender {
    pub(crate) fn send(&self, observation: RuntimeObservation) -> Result<(), ObservationSendError> {
        let observation_bytes = estimate_observation_bytes(&observation);
        let mut state = self.lock_state();
        if state.closed {
            return Err(ObservationSendError::Closed);
        }
        let result = if let Some(key) = live_key(&observation) {
            self.push_live(&mut state, key, Box::new(observation), observation_bytes)
        } else {
            self.push_lossless(
                &mut state,
                ObservationCommand::Event {
                    observation: Box::new(observation),
                    coalescing: CoalescingSummary::default(),
                },
            )
        };
        drop(state);
        self.queue.notify.notify_one();
        result
    }

    pub(crate) async fn flush(&self) -> Result<(), ObservationSendError> {
        let (sender, receiver) = oneshot::channel();
        {
            let mut state = self.lock_state();
            if state.closed {
                return Err(ObservationSendError::Closed);
            }
            self.push_lossless(&mut state, ObservationCommand::Flush(sender))?;
        }
        self.queue.notify.notify_one();
        receiver.await.map_err(|_| ObservationSendError::Closed)?
    }

    /// Close ingress and move the final pending live observations into order.
    pub(crate) fn close(&self) -> Result<(), ObservationSendError> {
        let mut state = self.lock_state();
        if state.closed {
            return Ok(());
        }
        self.close_state(&mut state);
        drop(state);
        self.queue.notify.notify_waiters();
        Ok(())
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, QueueState> {
        self.queue
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn push_live(
        &self,
        state: &mut QueueState,
        key: LiveKey,
        observation: Box<RuntimeObservation>,
        observation_bytes: usize,
    ) -> Result<(), ObservationSendError> {
        if let Some(index) = state
            .pending_live
            .iter()
            .position(|pending| pending.key == key)
        {
            let current_bytes = state
                .pending_live
                .get(index)
                .map_or(0, |pending| pending.bytes);
            if !fits_limits(state, 0, observation_bytes.saturating_sub(current_bytes)) {
                return Err(ObservationSendError::Overloaded);
            }
            // A replacement is a new ingress event. Move it to the back so a
            // coalesced provider stream cannot jump ahead of an intervening
            // tool/provider key that was observed later.
            let mut pending = state
                .pending_live
                .remove(index)
                .expect("live observation index must remain valid");
            let omitted_bytes = live_event_bytes(&pending.observation);
            pending.coalescing.omitted_events = pending.coalescing.omitted_events.saturating_add(1);
            pending.coalescing.omitted_bytes = pending
                .coalescing
                .omitted_bytes
                .saturating_add(omitted_bytes);
            pending.observation = observation;
            state.pending_live_bytes = state
                .pending_live_bytes
                .saturating_sub(current_bytes)
                .saturating_add(observation_bytes);
            pending.bytes = observation_bytes;
            state.pending_live.push_back(pending);
            return Ok(());
        }

        if !fits_limits(state, 1, observation_bytes) {
            return Err(ObservationSendError::Overloaded);
        }
        if state.pending_live.len() >= state.limits.max_pending_live
            && let Some(oldest) = state.pending_live.pop_front()
        {
            state.pending_live_bytes = state.pending_live_bytes.saturating_sub(oldest.bytes);
            self.enqueue_live(state, oldest);
        }
        state.pending_live.push_back(PendingLive {
            key,
            observation,
            coalescing: CoalescingSummary::default(),
            bytes: observation_bytes,
        });
        state.pending_live_bytes = state.pending_live_bytes.saturating_add(observation_bytes);
        Ok(())
    }

    fn push_lossless(
        &self,
        state: &mut QueueState,
        command: ObservationCommand,
    ) -> Result<(), ObservationSendError> {
        let bytes = command_bytes(&command);
        if !fits_limits(state, 1, bytes) {
            return Err(ObservationSendError::Overloaded);
        }
        self.flush_pending_live(state);
        state.commands.push_back(command);
        state.command_bytes = state.command_bytes.saturating_add(bytes);
        Ok(())
    }

    fn flush_pending_live(&self, state: &mut QueueState) {
        while let Some(pending) = state.pending_live.pop_front() {
            state.pending_live_bytes = state.pending_live_bytes.saturating_sub(pending.bytes);
            self.enqueue_live(state, pending);
        }
    }

    fn enqueue_live(&self, state: &mut QueueState, pending: PendingLive) {
        state.command_bytes = state.command_bytes.saturating_add(pending.bytes);
        state.commands.push_back(ObservationCommand::Event {
            observation: pending.observation,
            coalescing: pending.coalescing,
        });
    }

    fn close_state(&self, state: &mut QueueState) {
        state.closed = true;
        self.flush_pending_live(state);
    }
}

impl Drop for ObservationSender {
    fn drop(&mut self) {
        let mut state = self.lock_state();
        state.sender_count = state.sender_count.saturating_sub(1);
        let closed_by_drop = state.sender_count == 0 && !state.closed;
        if closed_by_drop {
            self.close_state(&mut state);
        }
        drop(state);
        if closed_by_drop {
            self.queue.notify.notify_waiters();
        }
    }
}

impl ObservationReceiver {
    pub(crate) async fn next(&self) -> Option<ObservationCommand> {
        loop {
            let notified = self.queue.notify.notified();
            match self.try_next() {
                QueuePoll::Command(command) => return Some(command),
                QueuePoll::Closed => return None,
                QueuePoll::Wait => {
                    // The notification future was registered before inspecting
                    // the state, so a concurrent sender cannot be missed here.
                    notified.await;
                }
            }
        }
    }

    fn try_next(&self) -> QueuePoll {
        let mut state = self
            .queue
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(command) = state.commands.pop_front() {
            state.command_bytes = state.command_bytes.saturating_sub(command_bytes(&command));
            return QueuePoll::Command(command);
        }
        if state.closed {
            return QueuePoll::Closed;
        }
        if let Some(pending) = state.pending_live.pop_front() {
            state.pending_live_bytes = state.pending_live_bytes.saturating_sub(pending.bytes);
            return QueuePoll::Command(ObservationCommand::Event {
                observation: pending.observation,
                coalescing: pending.coalescing,
            });
        }
        QueuePoll::Wait
    }
}

impl Drop for ObservationReceiver {
    fn drop(&mut self) {
        let mut state = self
            .queue
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        state.pending_live.clear();
        while let Some(command) = state.commands.pop_front() {
            if let ObservationCommand::Flush(sender) = command {
                let _ = sender.send(Err(ObservationSendError::Closed));
            }
        }
        drop(state);
        self.queue.notify.notify_waiters();
    }
}

enum QueuePoll {
    Command(ObservationCommand),
    Closed,
    Wait,
}

fn fits_limits(state: &QueueState, additional_commands: usize, additional_bytes: usize) -> bool {
    state
        .commands
        .len()
        .saturating_add(state.pending_live.len())
        .saturating_add(additional_commands)
        <= state.limits.max_commands
        && state
            .command_bytes
            .saturating_add(state.pending_live_bytes)
            .saturating_add(additional_bytes)
            <= state.limits.max_bytes
}

fn command_bytes(command: &ObservationCommand) -> usize {
    match command {
        ObservationCommand::Event { observation, .. } => estimate_observation_bytes(observation),
        ObservationCommand::Flush(_) => 0,
    }
}

fn estimate_observation_bytes(observation: &RuntimeObservation) -> usize {
    // RuntimeObservation intentionally remains a non-serializable execution
    // seam. Its Debug form includes all owned dynamic fields and gives this
    // queue a conservative, stable-enough accounting proxy without coupling
    // the recorder to every provider/tool payload type.
    format!("{observation:?}")
        .len()
        .saturating_add(std::mem::size_of::<RuntimeObservation>())
}

fn live_key(observation: &RuntimeObservation) -> Option<LiveKey> {
    match observation {
        RuntimeObservation::ProviderStreamed {
            request_id,
            provider_id,
            model_id,
            ..
        } => Some(LiveKey::ProviderStream(StreamKey {
            request_id: *request_id,
            provider_id: provider_id.clone(),
            model_id: model_id.clone(),
        })),
        RuntimeObservation::ToolProgress(ToolProgress { tool_call_id, .. }) => {
            Some(LiveKey::ToolProgress(*tool_call_id))
        }
        _ => None,
    }
}

fn live_event_bytes(observation: &RuntimeObservation) -> u64 {
    match observation {
        RuntimeObservation::ProviderStreamed { event, .. } => match event {
            ProviderStreamEvent::TextDelta { text }
            | ProviderStreamEvent::ReasoningDelta { text } => {
                u64::try_from(text.len()).unwrap_or(u64::MAX)
            }
            ProviderStreamEvent::ToolCallDelta {
                tool_call_id,
                tool_name,
                ..
            } => tool_call_id
                .as_deref()
                .unwrap_or_default()
                .len()
                .saturating_add(tool_name.as_deref().unwrap_or_default().len())
                .try_into()
                .unwrap_or(u64::MAX),
        },
        RuntimeObservation::ToolProgress(progress) => progress
            .detail
            .as_deref()
            .unwrap_or_default()
            .len()
            .saturating_add(progress.output_excerpt.as_deref().unwrap_or_default().len())
            .try_into()
            .unwrap_or(u64::MAX),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use golutra_core::{
        ProviderRequestId, ToolCallId, ToolProgress, ToolProgressPhase, ToolRecoveryPolicy,
    };
    use golutra_llm::ProviderStreamEvent;
    use golutra_runtime::RuntimeObservation;

    use super::*;

    fn streamed(text: &str) -> RuntimeObservation {
        RuntimeObservation::ProviderStreamed {
            request_id: ProviderRequestId::new(),
            provider_id: "provider".to_owned(),
            model_id: "model".to_owned(),
            event: ProviderStreamEvent::TextDelta {
                text: text.to_owned(),
            },
        }
    }

    fn streamed_with_request(request_id: ProviderRequestId, text: &str) -> RuntimeObservation {
        RuntimeObservation::ProviderStreamed {
            request_id,
            provider_id: "provider".to_owned(),
            model_id: "model".to_owned(),
            event: ProviderStreamEvent::TextDelta {
                text: text.to_owned(),
            },
        }
    }

    #[tokio::test]
    async fn same_provider_request_coalesces_to_latest_delta() {
        let (sender, receiver) = channel();
        let request_id = ProviderRequestId::new();
        sender
            .send(streamed_with_request(request_id, "first"))
            .expect("first delta");
        sender
            .send(streamed_with_request(request_id, "second"))
            .expect("second delta");
        sender.close().expect("close");

        let Some(ObservationCommand::Event {
            observation,
            coalescing,
        }) = receiver.next().await
        else {
            panic!("coalesced event");
        };
        assert!(matches!(
            *observation,
            RuntimeObservation::ProviderStreamed {
                event: ProviderStreamEvent::TextDelta { ref text },
                ..
            } if text == "second"
        ));
        assert_eq!(coalescing.omitted_events, 1);
        assert_eq!(coalescing.omitted_bytes, 5);
        assert!(receiver.next().await.is_none());
    }

    #[tokio::test]
    async fn critical_events_flush_pending_streams_in_order() {
        let (sender, receiver) = channel();
        sender.send(streamed("delta")).expect("stream");
        sender
            .send(RuntimeObservation::AssistantMessage {
                turn_id: golutra_core::TurnId::new(),
                content: "done".to_owned(),
            })
            .expect("assistant event");
        sender.close().expect("close");

        assert!(matches!(
            receiver.next().await,
            Some(ObservationCommand::Event {
                observation,
                coalescing: CoalescingSummary { omitted_events: 0, .. },
            }) if matches!(*observation, RuntimeObservation::ProviderStreamed { .. })
        ));
        assert!(matches!(
            receiver.next().await,
            Some(ObservationCommand::Event { observation, .. })
                if matches!(*observation, RuntimeObservation::AssistantMessage { .. })
        ));
    }

    #[tokio::test]
    async fn flush_ack_waits_until_prior_events_are_dequeued() {
        let (sender, receiver) = channel();
        sender.send(streamed("delta")).expect("stream");
        let flush = sender.flush();
        tokio::pin!(flush);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(5), &mut flush)
                .await
                .is_err()
        );
        assert!(matches!(
            receiver.next().await,
            Some(ObservationCommand::Event { .. })
        ));
        let Some(ObservationCommand::Flush(ack)) = receiver.next().await else {
            panic!("flush command");
        };
        ack.send(Ok(())).expect("ack");
        flush.await.expect("flush result");
        sender.close().expect("close");
    }

    #[tokio::test]
    async fn dropping_the_receiver_fails_pending_flushes_and_future_sends() {
        let (sender, receiver) = channel();
        let flush = sender.flush();
        drop(receiver);

        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), flush)
                .await
                .expect("flush must not hang")
                .expect_err("closed recorder must fail flush"),
            ObservationSendError::Closed
        );
        assert_eq!(
            sender.send(RuntimeObservation::AssistantMessage {
                turn_id: golutra_core::TurnId::new(),
                content: "late event".to_owned(),
            }),
            Err(ObservationSendError::Closed)
        );
    }

    #[tokio::test]
    async fn dropping_the_last_sender_closes_after_an_ordered_drain() {
        let (sender, receiver) = channel();
        let remaining_sender = sender.clone();
        sender.send(streamed("delta")).expect("stream");
        sender
            .send(RuntimeObservation::AssistantMessage {
                turn_id: golutra_core::TurnId::new(),
                content: "done".to_owned(),
            })
            .expect("assistant event");

        drop(sender);
        drop(remaining_sender);

        assert!(matches!(
            receiver.next().await,
            Some(ObservationCommand::Event { observation, .. })
                if matches!(*observation, RuntimeObservation::ProviderStreamed { .. })
        ));
        assert!(matches!(
            receiver.next().await,
            Some(ObservationCommand::Event { observation, .. })
                if matches!(*observation, RuntimeObservation::AssistantMessage { .. })
        ));
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), receiver.next())
                .await
                .expect("last sender drop must close the receiver")
                .is_none()
        );
    }

    #[tokio::test]
    async fn dropping_a_nonfinal_sender_keeps_ingress_open() {
        let (sender, receiver) = channel();
        let remaining_sender = sender.clone();

        drop(sender);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), receiver.next())
                .await
                .is_err(),
            "a remaining sender must keep the receiver open"
        );

        drop(remaining_sender);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), receiver.next())
                .await
                .expect("last sender drop must close the receiver")
                .is_none()
        );
    }

    #[tokio::test]
    async fn lossless_events_are_retained_when_live_lane_is_full() {
        let (sender, receiver) = channel();
        for _ in 0..(MAX_PENDING_LIVE_OBSERVATIONS + 1) {
            sender
                .send(streamed_with_request(ProviderRequestId::new(), "delta"))
                .expect("live event");
        }
        sender
            .send(RuntimeObservation::ProviderFailed {
                request_id: ProviderRequestId::new(),
                provider_id: "provider".to_owned(),
                model_id: "model".to_owned(),
                error: "terminal provider error".to_owned(),
            })
            .expect("lossless terminal event");
        sender.close().expect("close");

        let mut events = 0_usize;
        let mut terminal_seen = false;
        while let Some(command) = receiver.next().await {
            if let ObservationCommand::Event { observation, .. } = command {
                events = events.saturating_add(1);
                terminal_seen |= matches!(
                    *observation,
                    RuntimeObservation::ProviderFailed { ref error, .. }
                        if error == "terminal provider error"
                );
            }
        }
        assert!(terminal_seen);
        assert_eq!(events, MAX_PENDING_LIVE_OBSERVATIONS + 2);
    }

    #[tokio::test]
    async fn lossless_queue_reports_count_overload_instead_of_dropping_a_fact() {
        let (sender, receiver) = channel_with_limits(QueueLimits {
            max_commands: 2,
            max_bytes: usize::MAX,
            max_pending_live: 2,
        });
        for content in ["first", "second"] {
            sender
                .send(RuntimeObservation::AssistantMessage {
                    turn_id: golutra_core::TurnId::new(),
                    content: content.to_owned(),
                })
                .expect("event fits queue");
        }
        assert_eq!(
            sender.send(RuntimeObservation::AssistantMessage {
                turn_id: golutra_core::TurnId::new(),
                content: "third".to_owned(),
            }),
            Err(ObservationSendError::Overloaded)
        );

        sender.close().expect("close");
        let mut retained = 0;
        while let Some(ObservationCommand::Event { .. }) = receiver.next().await {
            retained += 1;
        }
        assert_eq!(retained, 2);
    }

    #[tokio::test]
    async fn queue_reports_byte_overload_before_mutating_pending_live_state() {
        let (sender, receiver) = channel_with_limits(QueueLimits {
            max_commands: 8,
            max_bytes: 1,
            max_pending_live: 2,
        });
        assert_eq!(
            sender.send(streamed("delta")),
            Err(ObservationSendError::Overloaded)
        );
        sender.close().expect("close");
        assert!(receiver.next().await.is_none());
    }

    #[tokio::test]
    async fn tool_progress_coalesces_by_tool_call_without_dropping_lossless_facts() {
        let (sender, receiver) = channel();
        let tool_call_id = ToolCallId::new();
        for phase in [
            ToolProgressPhase::Started,
            ToolProgressPhase::Output,
            ToolProgressPhase::Completed,
        ] {
            sender
                .send(RuntimeObservation::ToolProgress(ToolProgress {
                    tool_call_id,
                    tool_name: "shell".to_owned(),
                    phase,
                    elapsed_ms: 1,
                    output_bytes: 1,
                    output_lines: 1,
                    detail: Some(format!("{phase:?}")),
                    output_excerpt: None,
                }))
                .expect("progress");
        }
        sender
            .send(RuntimeObservation::ToolStarted {
                tool_call_id,
                provider_tool_call_id: None,
                tool_name: "shell".to_owned(),
                display_arguments: serde_json::json!({}),
                recovery_policy: ToolRecoveryPolicy::default(),
            })
            .expect("lossless lifecycle event");
        sender.close().expect("close");

        assert!(matches!(
            receiver.next().await,
            Some(ObservationCommand::Event {
                observation,
                coalescing: CoalescingSummary { omitted_events: 2, .. },
            }) if matches!(
                *observation,
                RuntimeObservation::ToolProgress(ToolProgress {
                    phase: ToolProgressPhase::Completed,
                    ..
                })
            )
        ));
        assert!(matches!(
            receiver.next().await,
            Some(ObservationCommand::Event { observation, .. })
                if matches!(*observation, RuntimeObservation::ToolStarted { .. })
        ));
        assert!(receiver.next().await.is_none());
    }

    #[tokio::test]
    async fn coalesced_live_keys_keep_their_latest_ingress_order() {
        let (sender, receiver) = channel();
        let first_request = ProviderRequestId::new();
        let second_request = ProviderRequestId::new();
        sender
            .send(streamed_with_request(first_request, "first-a"))
            .expect("first stream event");
        sender
            .send(streamed_with_request(second_request, "second"))
            .expect("second stream event");
        sender
            .send(streamed_with_request(first_request, "first-b"))
            .expect("replacement stream event");
        sender.close().expect("close");

        let Some(ObservationCommand::Event { observation, .. }) = receiver.next().await else {
            panic!("second stream event should remain first");
        };
        assert!(matches!(
            *observation,
            RuntimeObservation::ProviderStreamed {
                request_id,
                event: ProviderStreamEvent::TextDelta { ref text },
                ..
            } if request_id == second_request && text == "second"
        ));
        let Some(ObservationCommand::Event { observation, .. }) = receiver.next().await else {
            panic!("latest first stream event");
        };
        assert!(matches!(
            *observation,
            RuntimeObservation::ProviderStreamed {
                request_id,
                event: ProviderStreamEvent::TextDelta { ref text },
                ..
            } if request_id == first_request && text == "first-b"
        ));
        assert!(receiver.next().await.is_none());
    }
}
