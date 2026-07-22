//! Durable-friendly step bookkeeping for long-running agent turns.
//!
//! The machine deliberately does not decide whether a task is safe to run. That
//! remains the governor's responsibility. It records progress boundaries and
//! detects a loop which keeps producing the same action without making progress.

use golutra_core::TurnId;
use serde::{Deserialize, Serialize};

pub const DEFAULT_NO_PROGRESS_LIMIT: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepSnapshot {
    pub step_no: u32,
    pub turn_id: TurnId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepCheckpoint {
    pub next_step_no: u32,
    pub last_fingerprint: Option<String>,
    pub repeated_no_progress: u32,
    pub no_progress_limit: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepCompletion {
    pub snapshot: StepSnapshot,
    pub fingerprint: String,
    pub made_progress: bool,
    pub repeated_no_progress: u32,
    pub should_stop: bool,
}

#[derive(Debug, Clone)]
pub struct StepMachine {
    next_step_no: u32,
    last_fingerprint: Option<String>,
    repeated_no_progress: u32,
    no_progress_limit: u32,
}

impl Default for StepMachine {
    fn default() -> Self {
        Self::new(DEFAULT_NO_PROGRESS_LIMIT)
    }
}

impl StepMachine {
    #[must_use]
    pub fn new(no_progress_limit: u32) -> Self {
        Self {
            next_step_no: 0,
            last_fingerprint: None,
            repeated_no_progress: 0,
            no_progress_limit: no_progress_limit.max(1),
        }
    }

    #[must_use]
    pub fn begin(&mut self, turn_id: TurnId) -> StepSnapshot {
        let snapshot = StepSnapshot {
            step_no: self.next_step_no,
            turn_id,
        };
        self.next_step_no = self.next_step_no.saturating_add(1);
        snapshot
    }

    #[must_use]
    pub fn complete(
        &mut self,
        snapshot: StepSnapshot,
        fingerprint: impl Into<String>,
        made_progress: bool,
    ) -> StepCompletion {
        let fingerprint = fingerprint.into();
        if made_progress {
            self.repeated_no_progress = 0;
        } else if self.last_fingerprint.as_deref() != Some(fingerprint.as_str()) {
            self.repeated_no_progress = 1;
        } else {
            self.repeated_no_progress = self.repeated_no_progress.saturating_add(1);
        }
        self.last_fingerprint = Some(fingerprint.clone());
        StepCompletion {
            snapshot,
            fingerprint,
            made_progress,
            repeated_no_progress: self.repeated_no_progress,
            should_stop: self.repeated_no_progress >= self.no_progress_limit,
        }
    }

    #[must_use]
    pub fn checkpoint(&self) -> StepCheckpoint {
        StepCheckpoint {
            next_step_no: self.next_step_no,
            last_fingerprint: self.last_fingerprint.clone(),
            repeated_no_progress: self.repeated_no_progress,
            no_progress_limit: self.no_progress_limit,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_identical_steps_are_stopped_after_the_configured_limit() {
        let turn_id = TurnId::new();
        let mut machine = StepMachine::new(2);

        let first_step = machine.begin(turn_id);
        let first = machine.complete(first_step, "same-action", false);
        let second_step = machine.begin(turn_id);
        let second = machine.complete(second_step, "same-action", false);
        let third_step = machine.begin(turn_id);
        let third = machine.complete(third_step, "same-action", false);

        assert_eq!(first.repeated_no_progress, 1);
        assert!(second.should_stop);
        assert!(third.should_stop);
    }

    #[test]
    fn successful_steps_reset_no_progress() {
        let turn_id = TurnId::new();
        let mut machine = StepMachine::new(2);

        let first_step = machine.begin(turn_id);
        let _ = machine.complete(first_step, "same-action", false);
        let second_step = machine.begin(turn_id);
        let _ = machine.complete(second_step, "same-action", false);
        let third_step = machine.begin(turn_id);
        let completion = machine.complete(third_step, "same-action", true);

        assert_eq!(completion.repeated_no_progress, 0);
        assert!(!completion.should_stop);
    }
}
