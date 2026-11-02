//! Routing of pooled transactions to the store that holds them.

use crate::BasePooledTx;

/// The store inside [`crate::BaseTransactionPool`] that owns a transaction.
///
/// Every transaction carrying validity predicates lives in the validity pool,
/// whatever its type or nonce mode. Among the rest, EIP-8130 transactions
/// sequenced on a finite nonce channel or nonce-free live in the sidecar, and
/// everything sequenced on the account's protocol nonce lives in the reth
/// protocol pool. A `(sender, nonce key, nonce)` slot is held by at most one
/// store at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PoolStore {
    /// The reth `Pool` holding standard, protocol-nonce transactions.
    Protocol,
    /// The EIP-8130 sidecar holding standard channelized and nonce-free transactions.
    Sidecar,
    /// The validity pool holding every transaction with validity predicates.
    Validity,
}

impl PoolStore {
    /// Every store, in the order the merged best-transaction iterator uses.
    pub const ALL: [Self; 3] = [Self::Protocol, Self::Sidecar, Self::Validity];

    /// Returns the store `transaction` belongs in.
    pub fn of<T: BasePooledTx>(transaction: &T) -> Self {
        if !transaction.validity_conditions().is_empty() {
            Self::Validity
        } else if transaction.is_eip8130_sidecar_transaction() {
            Self::Sidecar
        } else {
            Self::Protocol
        }
    }

    /// Position of this store in [`Self::ALL`].
    pub const fn index(self) -> usize {
        match self {
            Self::Protocol => 0,
            Self::Sidecar => 1,
            Self::Validity => 2,
        }
    }

    /// Whether this store keeps its own, non-protocol-pool lanes.
    pub const fn is_lane_store(self) -> bool {
        !matches!(self, Self::Protocol)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_matches_all_order() {
        for (index, store) in PoolStore::ALL.into_iter().enumerate() {
            assert_eq!(store.index(), index);
        }
    }
}
