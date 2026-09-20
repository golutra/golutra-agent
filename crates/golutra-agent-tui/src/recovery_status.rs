//! 从恢复事件重建等待状态和统计，避免重试通知不断占用聊天正文。

use chrono::{DateTime, Utc};
use golutra_agent_protocol::{RuntimeEvent, RuntimeEventType};

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RecoverySnapshot {
    pub network: bool,
    pub connecting: bool,
    pub attempt: u64,
    pub remaining_seconds: u64,
    pub waited_seconds: u64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RecoveryActivity {
    current: Option<RecoverySnapshot>,
    wait_started: Option<DateTime<Utc>>,
    wait_ms: u64,
    delay_ms: u64,
    retries: u64,
    last_progress: Option<DateTime<Utc>>,
}

impl RecoveryActivity {
    pub fn apply(&mut self, event: &RuntimeEvent) {
        if let Some(recovery) = event.payload.get("recovery")
            && event.event_type == RuntimeEventType::RetryScheduled
        {
            self.finish_wait(event.timestamp);
            let waiting = recovery["phase"].as_str() == Some("waiting");
            if waiting {
                self.retries += 1;
                self.wait_started = Some(event.timestamp);
            }
            self.delay_ms = recovery["delay_ms"].as_u64().unwrap_or_default();
            self.current = Some(RecoverySnapshot {
                network: recovery["network"].as_bool().unwrap_or(false),
                connecting: !waiting,
                attempt: recovery["attempt"].as_u64().unwrap_or_default(),
                remaining_seconds: 0,
                waited_seconds: 0,
            });
            return;
        }
        if matches!(
            event.event_type,
            RuntimeEventType::ProviderStreamed
                | RuntimeEventType::ProviderCompleted
                | RuntimeEventType::ToolCompleted
        ) {
            self.last_progress = Some(event.timestamp);
        }
        if matches!(
            event.event_type,
            RuntimeEventType::ProviderStarted
                | RuntimeEventType::ProviderStreamed
                | RuntimeEventType::ProviderCompleted
                | RuntimeEventType::ProviderFailed
        ) || event.event_type.is_task_terminal()
        {
            self.finish_wait(event.timestamp);
            self.current = None;
        }
    }

    fn pending_ms(&self, now: DateTime<Utc>) -> u64 {
        self.wait_started
            .map_or(0, |start| (now - start).num_milliseconds().max(0) as u64)
    }

    fn finish_wait(&mut self, now: DateTime<Utc>) {
        self.wait_ms = self.wait_ms.saturating_add(self.pending_ms(now));
        self.wait_started = None;
    }

    pub fn snapshot(&self, now: DateTime<Utc>) -> Option<RecoverySnapshot> {
        self.current.map(|mut snapshot| {
            snapshot.remaining_seconds = self
                .delay_ms
                .saturating_sub(self.pending_ms(now))
                .div_ceil(1_000);
            snapshot.waited_seconds = self.wait_ms.saturating_add(self.pending_ms(now)) / 1_000;
            snapshot
        })
    }

    pub fn details(&self, now: DateTime<Utc>) -> Vec<String> {
        let mut lines = vec![format!(
            "provider retries {} · retry wait {}s",
            self.retries,
            self.wait_ms.saturating_add(self.pending_ms(now)) / 1_000
        )];
        if let Some(last) = self.last_progress {
            lines.push(format!("last progress {}", last.to_rfc3339()));
        }
        if let Some(state) = self.snapshot(now) {
            lines.push(format!(
                "{} · attempt {} · next retry in {}s",
                if state.connecting {
                    "reconnecting"
                } else if state.network {
                    "waiting for network"
                } else {
                    "waiting to retry"
                },
                state.attempt,
                if state.connecting {
                    0
                } else {
                    state.remaining_seconds
                }
            ));
        }
        lines
    }
}
