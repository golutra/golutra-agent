//! Compatibility re-exports for the former event crate.
//!
//! New code should depend on `golutra-protocol` directly. This small shim keeps
//! existing workspace and SDK integrations source-compatible while the event
//! contracts live in their canonical protocol crate.

pub use golutra_protocol::{EventFilter, RuntimeEvent, RuntimeEventSource, RuntimeEventType};

#[must_use]
pub fn crate_role() -> &'static str {
    "compatibility exports for durable and live runtime event contracts"
}
