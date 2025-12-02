pub mod rpc;

use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub enum Status {
    Unknown,
    Known,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub struct TransactionStatusResponse {
    pub status: Status,
}
