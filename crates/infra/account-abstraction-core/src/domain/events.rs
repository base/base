use crate::domain::types::WrappedUserOperation;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data")]
pub enum MempoolEvent {
    UserOpAdded {
        user_op: WrappedUserOperation,
    },
    UserOpIncluded {
        user_op: WrappedUserOperation,
    },
    UserOpDropped {
        user_op: WrappedUserOperation,
        reason: String,
    },
}
