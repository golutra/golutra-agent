//! Durable-friendly step bookkeeping for long-running agent turns.
//!
//! The machine deliberately does not decide whether a task is safe to run. That
//! remains the governor's responsibility. It records progress boundaries and
//! detects a loop which keeps producing the same semantic action without making progress.

use golutra_core::TurnId;
use serde::{Deserialize, Serialize};

pub const DEFAULT_NO_PROGRESS_ADVISORY_LIMIT: u32 = 3;
pub const DEFAULT_NO_PROGRESS_LIMIT: u32 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CorrectionProgressLimits {
    pub step_limit: u32,
    pub elapsed_ms_limit: u64,
}

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
    #[serde(default = "default_no_progress_advisory_limit")]
    pub no_progress_advisory_limit: u32,
    pub no_progress_limit: u32,
    #[serde(default)]
    pub correction_active: bool,
    #[serde(default)]
    pub correction_no_progress_steps: u32,
    #[serde(default)]
    pub correction_no_progress_elapsed_ms: u64,
    #[serde(default)]
    pub correction_no_progress_step_limit: u32,
    #[serde(default)]
    pub correction_no_progress_elapsed_ms_limit: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepCompletion {
    pub snapshot: StepSnapshot,
    pub fingerprint: String,
    /// The step changed the semantic action and should reset repetition guards.
    pub made_progress: bool,
    /// The step changed durable state or produced passing validation evidence.
    pub made_material_progress: bool,
    pub repeated_no_progress: u32,
    pub correction_no_progress_steps: u32,
    pub correction_no_progress_elapsed_ms: u64,
    pub advisory: Option<String>,
    pub should_stop: bool,
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StepMachine {
    next_step_no: u32,
    last_fingerprint: Option<String>,
    repeated_no_progress: u32,
    no_progress_advisory_limit: u32,
    no_progress_limit: u32,
    correction_limits: CorrectionProgressLimits,
    correction_active: bool,
    correction_no_progress_steps: u32,
    correction_last_material_progress_ms: u64,
    correction_no_progress_elapsed_ms: u64,
    correction_advisory_emitted: bool,
}

impl Default for StepMachine {
    fn default() -> Self {
        Self::with_limits(
            DEFAULT_NO_PROGRESS_ADVISORY_LIMIT,
            DEFAULT_NO_PROGRESS_LIMIT,
            CorrectionProgressLimits {
                step_limit: 0,
                elapsed_ms_limit: 0,
            },
        )
    }
}

impl StepMachine {
    #[must_use]
    #[cfg(test)]
    pub fn new(no_progress_limit: u32) -> Self {
        let no_progress_limit = no_progress_limit.max(1);
        Self::with_limits(
            no_progress_limit.saturating_sub(1).max(1),
            no_progress_limit,
            CorrectionProgressLimits {
                step_limit: 0,
                elapsed_ms_limit: 0,
            },
        )
    }

    #[must_use]
    pub fn with_limits(
        advisory_limit: u32,
        stop_limit: u32,
        correction_limits: CorrectionProgressLimits,
    ) -> Self {
        let stop_limit = stop_limit.max(1);
        Self {
            next_step_no: 0,
            last_fingerprint: None,
            repeated_no_progress: 0,
            no_progress_advisory_limit: advisory_limit.clamp(1, stop_limit),
            no_progress_limit: stop_limit,
            correction_limits,
            correction_active: false,
            correction_no_progress_steps: 0,
            correction_last_material_progress_ms: 0,
            correction_no_progress_elapsed_ms: 0,
            correction_advisory_emitted: false,
        }
    }

    pub fn begin_correction(&mut self, elapsed_ms: u64) {
        if self.correction_active {
            return;
        }
        self.correction_active = true;
        self.correction_no_progress_steps = 0;
        self.correction_last_material_progress_ms = elapsed_ms;
        self.correction_no_progress_elapsed_ms = 0;
        self.correction_advisory_emitted = false;
    }

    pub fn end_correction(&mut self) {
        self.correction_active = false;
        self.correction_no_progress_steps = 0;
        self.correction_no_progress_elapsed_ms = 0;
        self.correction_advisory_emitted = false;
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
    #[cfg(test)]
    pub fn complete_at(
        &mut self,
        snapshot: StepSnapshot,
        fingerprint: impl Into<String>,
        made_progress: bool,
        elapsed_ms: u64,
    ) -> StepCompletion {
        self.complete_at_with_material_progress(
            snapshot,
            fingerprint,
            made_progress,
            made_progress,
            elapsed_ms,
        )
    }

    #[must_use]
    pub fn complete_at_with_material_progress(
        &mut self,
        snapshot: StepSnapshot,
        fingerprint: impl Into<String>,
        made_progress: bool,
        made_material_progress: bool,
        elapsed_ms: u64,
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
        let semantic_advisory =
            (self.repeated_no_progress == self.no_progress_advisory_limit).then(|| {
                format!(
                    "runtime has made no observable progress for {} semantically equivalent steps; execution remains active until the hard limit of {}",
                    self.repeated_no_progress, self.no_progress_limit
                )
            });
        let semantic_stop = self.repeated_no_progress >= self.no_progress_limit;
        let mut correction_advisory = None;
        let mut correction_stop = false;
        if self.correction_active {
            if made_material_progress {
                self.correction_no_progress_steps = 0;
                self.correction_last_material_progress_ms = elapsed_ms;
                self.correction_no_progress_elapsed_ms = 0;
                self.correction_advisory_emitted = false;
            } else {
                self.correction_no_progress_steps =
                    self.correction_no_progress_steps.saturating_add(1);
                self.correction_no_progress_elapsed_ms =
                    elapsed_ms.saturating_sub(self.correction_last_material_progress_ms);
            }
            correction_stop = limit_reached(
                self.correction_no_progress_steps,
                self.correction_limits.step_limit,
            ) || limit_reached(
                self.correction_no_progress_elapsed_ms,
                self.correction_limits.elapsed_ms_limit,
            );
            let correction_advisory_reached = limit_reached(
                self.correction_no_progress_steps,
                advisory_limit(self.correction_limits.step_limit),
            ) || limit_reached(
                self.correction_no_progress_elapsed_ms,
                advisory_limit(self.correction_limits.elapsed_ms_limit),
            );
            if correction_advisory_reached && !correction_stop && !self.correction_advisory_emitted
            {
                self.correction_advisory_emitted = true;
                correction_advisory = Some(format!(
                    "verification correction has made no material progress for {} steps ({} ms); finish verification or change the workspace before the correction budget is exhausted",
                    self.correction_no_progress_steps, self.correction_no_progress_elapsed_ms
                ));
            }
        }
        let stop_reason = if correction_stop {
            Some(format!(
                "verification correction made no material progress for {} steps ({} ms)",
                self.correction_no_progress_steps, self.correction_no_progress_elapsed_ms
            ))
        } else if semantic_stop {
            Some(format!(
                "runtime made no observable progress for {} semantically equivalent steps",
                self.repeated_no_progress
            ))
        } else {
            None
        };
        StepCompletion {
            snapshot,
            fingerprint,
            made_progress,
            made_material_progress,
            repeated_no_progress: self.repeated_no_progress,
            correction_no_progress_steps: self.correction_no_progress_steps,
            correction_no_progress_elapsed_ms: self.correction_no_progress_elapsed_ms,
            advisory: correction_advisory.or(semantic_advisory),
            should_stop: correction_stop || semantic_stop,
            stop_reason,
        }
    }

    #[cfg(test)]
    #[must_use]
    pub fn complete(
        &mut self,
        snapshot: StepSnapshot,
        fingerprint: impl Into<String>,
        made_progress: bool,
    ) -> StepCompletion {
        self.complete_at(snapshot, fingerprint, made_progress, 0)
    }

    #[must_use]
    pub fn checkpoint(&self) -> StepCheckpoint {
        StepCheckpoint {
            next_step_no: self.next_step_no,
            last_fingerprint: self.last_fingerprint.clone(),
            repeated_no_progress: self.repeated_no_progress,
            no_progress_advisory_limit: self.no_progress_advisory_limit,
            no_progress_limit: self.no_progress_limit,
            correction_active: self.correction_active,
            correction_no_progress_steps: self.correction_no_progress_steps,
            correction_no_progress_elapsed_ms: self.correction_no_progress_elapsed_ms,
            correction_no_progress_step_limit: self.correction_limits.step_limit,
            correction_no_progress_elapsed_ms_limit: self.correction_limits.elapsed_ms_limit,
        }
    }
}

fn limit_reached<T>(value: T, limit: T) -> bool
where
    T: Copy + Default + PartialEq + PartialOrd,
{
    limit != T::default() && value >= limit
}

fn advisory_limit<T>(limit: T) -> T
where
    T: Copy + Default + From<u8> + PartialEq + std::ops::Div<Output = T>,
{
    if limit == T::default() {
        limit
    } else {
        limit / T::from(2_u8)
    }
}

const fn default_no_progress_advisory_limit() -> u32 {
    DEFAULT_NO_PROGRESS_ADVISORY_LIMIT
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

    #[test]
    fn default_policy_advises_before_it_stops() {
        let turn_id = TurnId::new();
        let mut machine = StepMachine::default();
        let mut completions = Vec::new();

        for _ in 0..DEFAULT_NO_PROGRESS_LIMIT {
            let step = machine.begin(turn_id);
            completions.push(machine.complete(step, "same-action", false));
        }

        assert!(completions[2].advisory.is_some());
        assert!(!completions[2].should_stop);
        assert!(completions[5].should_stop);
    }

    #[test]
    fn correction_progress_is_bounded_without_limiting_normal_exploration() {
        let turn_id = TurnId::new();
        let mut machine = StepMachine::with_limits(
            DEFAULT_NO_PROGRESS_ADVISORY_LIMIT,
            DEFAULT_NO_PROGRESS_LIMIT,
            CorrectionProgressLimits {
                step_limit: 4,
                elapsed_ms_limit: 1_000,
            },
        );

        let exploration = machine.begin(turn_id);
        let exploration = machine.complete_at(exploration, "explore", false, 5_000);
        assert!(!exploration.should_stop);
        assert_eq!(exploration.correction_no_progress_steps, 0);

        machine.begin_correction(5_000);
        let first = machine.begin(turn_id);
        let first = machine.complete_at(first, "correct-a", false, 5_100);
        let second = machine.begin(turn_id);
        let second = machine.complete_at(second, "correct-b", false, 5_500);
        let third = machine.begin(turn_id);
        let third = machine.complete_at(third, "correct-c", false, 6_000);

        assert!(!first.should_stop);
        assert!(second.advisory.is_some());
        assert!(third.should_stop);
        assert_eq!(third.correction_no_progress_steps, 3);
        assert_eq!(third.correction_no_progress_elapsed_ms, 1_000);
        assert!(
            third
                .stop_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("verification correction"))
        );
    }

    #[test]
    fn material_progress_resets_the_correction_window() {
        let turn_id = TurnId::new();
        let mut machine = StepMachine::with_limits(
            DEFAULT_NO_PROGRESS_ADVISORY_LIMIT,
            DEFAULT_NO_PROGRESS_LIMIT,
            CorrectionProgressLimits {
                step_limit: 3,
                elapsed_ms_limit: 1_000,
            },
        );
        machine.begin_correction(100);

        let first = machine.begin(turn_id);
        let _ = machine.complete_at(first, "inspect-a", false, 700);
        let changed = machine.begin(turn_id);
        let changed = machine.complete_at(changed, "edit", true, 900);
        let after_change = machine.begin(turn_id);
        let after_change = machine.complete_at(after_change, "inspect-b", false, 1_500);

        assert_eq!(changed.correction_no_progress_steps, 0);
        assert_eq!(changed.correction_no_progress_elapsed_ms, 0);
        assert_eq!(after_change.correction_no_progress_steps, 1);
        assert_eq!(after_change.correction_no_progress_elapsed_ms, 600);
        assert!(!after_change.should_stop);
    }

    #[test]
    fn repeated_correction_cycles_preserve_the_active_no_progress_window() {
        let turn_id = TurnId::new();
        let mut machine = StepMachine::with_limits(
            DEFAULT_NO_PROGRESS_ADVISORY_LIMIT,
            DEFAULT_NO_PROGRESS_LIMIT,
            CorrectionProgressLimits {
                step_limit: 3,
                elapsed_ms_limit: 1_000,
            },
        );
        machine.begin_correction(100);
        let first = machine.begin(turn_id);
        let first = machine.complete_at(first, "inspect-a", false, 500);

        machine.begin_correction(700);
        let second = machine.begin(turn_id);
        let second = machine.complete_at(second, "inspect-b", false, 900);

        assert_eq!(first.correction_no_progress_steps, 1);
        assert_eq!(second.correction_no_progress_steps, 2);
        assert_eq!(second.correction_no_progress_elapsed_ms, 800);
    }
}
