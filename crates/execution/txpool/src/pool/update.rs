//! Support types for updating the pool.

use std::sync::Arc;

use crate::{SubPool, ValidPoolTransaction, identifier::TransactionId};

/// A change of the transaction's location
///
/// NOTE: this guarantees that `current` and `destination` differ.
#[derive(Debug)]
pub struct PoolUpdate {
    /// Internal tx id.
    pub id: TransactionId,
    /// Where the transaction is currently held.
    pub current: SubPool,
    /// Where to move the transaction to.
    pub destination: Destination,
}

/// Where to move an existing transaction.
#[derive(Debug)]
pub enum Destination {
    /// Discard the transaction.
    Discard,
    /// Move transaction to pool
    Pool(SubPool),
}

impl From<SubPool> for Destination {
    fn from(sub_pool: SubPool) -> Self {
        Self::Pool(sub_pool)
    }
}

/// Tracks the result after updating the pool
#[derive(Debug)]
pub struct UpdateOutcome {
    /// transactions promoted to the pending pool
    pub promoted: Vec<Arc<ValidPoolTransaction>>,
    /// transaction that failed and were discarded
    pub discarded: Vec<Arc<ValidPoolTransaction>>,
}

impl Default for UpdateOutcome {
    fn default() -> Self {
        Self { promoted: vec![], discarded: vec![] }
    }
}
