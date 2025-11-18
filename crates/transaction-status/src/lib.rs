mod proxy;
mod rpc;

pub use proxy::TransactionStatusProxyImpl;
pub use rpc::{TransactionStatusApiImpl, TransactionStatusApiServer};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub enum Status {
    Unknown,
    Pending,
    Queued,
    BlockIncluded,
    BuilderIncluded,
    Cancelled,
    Dropped,
}
