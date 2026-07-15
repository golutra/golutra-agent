mod model;
mod store;
mod validate;

pub use model::*;
pub use store::PluginStore;

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("plugin IO failed: {0}")]
    Io(String),
    #[error("plugin JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("plugin manifest is invalid: {0}")]
    InvalidManifest(String),
    #[error("plugin package limit exceeded: {0}")]
    Limit(String),
    #[error("plugin not found: {0}")]
    NotFound(String),
    #[error("plugin revision not found: {0}")]
    RevisionNotFound(String),
    #[error("plugin lifecycle state is invalid: {0}")]
    InvalidState(String),
    #[error("plugin package integrity check failed: {0}")]
    Integrity(String),
}

#[cfg(test)]
mod tests;
