use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const RUNTIME_PROTOCOL_NAME: &str = "golutra-runtime";
pub const RUNTIME_PROTOCOL_VERSION: u32 = 8;
pub const MINIMUM_RUNTIME_PROTOCOL_VERSION: u32 = 7;
pub const VERSIONED_WIRE_PROTOCOL_VERSION: u32 = 8;
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

    #[must_use]
    pub const fn highest_common(self, peer: Self) -> Option<u32> {
        let minimum = if self.minimum > peer.minimum {
            self.minimum
        } else {
            peer.minimum
        };
        let current = if self.current < peer.current {
            self.current
        } else {
            peer.current
        };
        if minimum <= current {
            Some(current)
        } else {
            None
        }
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
        assert!(range.accepts(MINIMUM_RUNTIME_PROTOCOL_VERSION));
        assert!(!range.accepts(3));
        assert!(!range.accepts(0));
        assert!(!range.accepts(RUNTIME_PROTOCOL_VERSION + 1));
    }

    #[test]
    fn protocol_negotiation_selects_the_highest_common_version() {
        let local = ProtocolVersionRange::runtime();

        assert_eq!(
            local.highest_common(ProtocolVersionRange {
                minimum: 7,
                current: 7,
            }),
            Some(7)
        );
        assert_eq!(
            local.highest_common(ProtocolVersionRange {
                minimum: 8,
                current: 9,
            }),
            Some(8)
        );
        assert_eq!(
            local.highest_common(ProtocolVersionRange {
                minimum: 9,
                current: 10,
            }),
            None
        );
    }
}
