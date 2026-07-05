pub use golutra_protocol::{EventFilter, RuntimeEvent, RuntimeEventSource, RuntimeEventType};

#[must_use]
pub fn crate_role() -> &'static str {
    "durable and live runtime event contracts"
}
