use serde::{Deserialize, Serialize};

use crate::domain::types::WrappedUserOperation;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data")]
pub enum MempoolEvent {
    UserOpAdded { user_op: WrappedUserOperation },
    UserOpIncluded { user_op: WrappedUserOperation },
    UserOpDropped { user_op: WrappedUserOperation, reason: String },
}
