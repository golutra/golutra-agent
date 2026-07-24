mod model;
mod runner;
mod store;

pub use golutra_core::EvaluationPartitionKind;
pub use model::*;
pub use runner::{
    EvaluationRunner, benchmark_run_has_required_metadata, candidate_mutates_control_plane,
    decide_governed_promotion, decide_low_risk_promotion, improvement_candidate_from_failure,
    replay_summary,
};
pub use store::{EvaluationError, EvaluationStore};

#[cfg(test)]
mod tests;
