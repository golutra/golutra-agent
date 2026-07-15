mod model;
mod planner;
mod store;

pub use model::*;
pub use planner::EvolutionPlanner;
pub use store::EvolutionStore;

#[derive(Debug, thiserror::Error)]
pub enum EvolutionError {
    #[error("evolution IO failed: {0}")]
    Io(String),
    #[error("evolution JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("evolution state limit exceeded: {0}")]
    Limit(String),
    #[error("skill candidate failed promotion gate: {0}")]
    SkillGate(String),
    #[error("skill candidate not found: {0}")]
    SkillNotFound(String),
    #[error("evolution run not found: {0}")]
    RunNotFound(String),
    #[error("skill lifecycle state is invalid: {0}")]
    SkillState(String),
}

#[cfg(test)]
mod tests;
