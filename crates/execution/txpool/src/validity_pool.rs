//! Configuration and classification for the separate validity transaction store.

use reth_transaction_pool::PoolConfig;

use crate::BasePooledTx;

/// Which store of [`crate::BaseTransactionPool`] owns a transaction.
///
/// A transaction with any submitted validity predicate belongs to the validity store, whatever
/// its nonce shape. Every other transaction belongs to the normal stores (the protocol pool and
/// the EIP-8130 sidecar), which keep their existing behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransactionClass {
    /// A transaction without validity predicates.
    Normal,
    /// A transaction carrying validity predicates.
    Validity,
}

impl TransactionClass {
    /// Returns the class of a pooled transaction.
    pub fn of<T: BasePooledTx>(transaction: &T) -> Self {
        if transaction.validity_conditions().is_empty() { Self::Normal } else { Self::Validity }
    }

    /// Returns whether a pooled transaction belongs to the validity store.
    pub fn is_validity<T: BasePooledTx>(transaction: &T) -> bool {
        Self::of(transaction) == Self::Validity
    }
}

/// Capacity of the validity store, independent of the normal pool's allowances.
///
/// The count and byte caps cover every validity transaction, pending or queued, across all
/// nonce shapes. The slot cap bounds how many transactions one non-local sender may hold there.
/// The fee floor, locality policy, replacement price bumps and delegated-account slot limit are
/// inherited from the normal pool configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidityPoolConfig {
    /// Maximum validity transactions held.
    pub max_count: usize,
    /// Maximum summed size of validity transactions, in bytes.
    pub max_size_bytes: usize,
    /// Maximum validity transactions one non-local sender may hold.
    pub max_account_slots: usize,
}

impl ValidityPoolConfig {
    /// Copies the normal pool's pending count and byte caps and its account slot cap, as a
    /// separate allowance.
    pub const fn from_pool_config(config: &PoolConfig) -> Self {
        Self {
            max_count: config.pending_limit.max_txs,
            max_size_bytes: config.pending_limit.max_size,
            max_account_slots: config.max_account_slots,
        }
    }

    /// Replaces each limit that has an override.
    #[must_use]
    pub fn with_overrides(
        mut self,
        max_count: Option<usize>,
        max_size_bytes: Option<usize>,
        max_account_slots: Option<usize>,
    ) -> Self {
        self.max_count = max_count.unwrap_or(self.max_count);
        self.max_size_bytes = max_size_bytes.unwrap_or(self.max_size_bytes);
        self.max_account_slots = max_account_slots.unwrap_or(self.max_account_slots);
        self
    }
}
