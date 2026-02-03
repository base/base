//! Contains the [`StoredBackrunBundle`] type.

use uuid::Uuid;
use alloy_primitives::Address;
use reth_optimism_txpool::OpPooledTransaction;

/// A backrun bundle that's stored.
#[derive(Clone, Debug)]
pub struct StoredBackrunBundle {
    /// The ID for the bundle.
    pub bundle_id: Uuid,
    /// The bundle sender.
    pub sender: Address,
    /// The backrun transactions in the bundle.
    pub backrun_txs: Vec<OpPooledTransaction>,
    /// The total priority fee for the bundle.
    pub total_priority_fee: u128,
}

