use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! define_id {
    ($name:ident) => {
        #[derive(
            Debug,
            Clone,
            Copy,
            PartialEq,
            Eq,
            Hash,
            PartialOrd,
            Ord,
            Serialize,
            Deserialize,
            JsonSchema,
        )]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "{}", self.0)
            }
        }
    };
}

define_id!(WorkspaceId);
define_id!(SessionId);
define_id!(TaskId);
define_id!(TurnId);
define_id!(CommandId);
define_id!(QueryId);
define_id!(EventId);
define_id!(LaneId);
define_id!(DecisionId);
define_id!(ToolCallId);
define_id!(ArtifactId);
define_id!(EvidenceId);
define_id!(VerificationId);
define_id!(PolicyId);
define_id!(CheckpointId);
define_id!(ProviderRequestId);
define_id!(ProviderResponseId);
define_id!(TokenBudgetSnapshotId);
define_id!(TokenUsageRecordId);
define_id!(LoopDecisionId);
