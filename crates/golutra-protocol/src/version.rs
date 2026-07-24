use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const RUNTIME_PROTOCOL_NAME: &str = "golutra-runtime";
pub const RUNTIME_PROTOCOL_VERSION: u32 = 5;
pub const MINIMUM_RUNTIME_PROTOCOL_VERSION: u32 = 5;
pub const RUNTIME_STATE_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProtocolVersionRange {
    pub minimum: u32,
    pub current: u32,
}

impl ProtocolVersionRange {
    #[must_use]
    pub const fn runtime() -> Self {
        Self {
            minimum: MINIMUM_RUNTIME_PROTOCOL_VERSION,
            current: RUNTIME_PROTOCOL_VERSION,
        }
    }

    #[must_use]
    pub const fn accepts(self, version: u32) -> bool {
        version >= self.minimum && version <= self.current
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProtocolHandshake {
    pub name: String,
    pub versions: ProtocolVersionRange,
}

impl ProtocolHandshake {
    #[must_use]
    pub fn runtime() -> Self {
        Self {
            name: RUNTIME_PROTOCOL_NAME.to_owned(),
            versions: ProtocolVersionRange::runtime(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_protocol_range_rejects_unknown_versions() {
        let range = ProtocolVersionRange::runtime();
        assert!(range.accepts(RUNTIME_PROTOCOL_VERSION));
        assert!(!range.accepts(3));
        assert!(!range.accepts(0));
        assert!(!range.accepts(RUNTIME_PROTOCOL_VERSION + 1));
    }
}
