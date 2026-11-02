//! Capacity limits for the validity pool.

use reth_transaction_pool::{PoolConfig, SubPoolLimit};

use crate::two_d_nonce_pool::SidecarLimits;

/// Operator-facing limits for the validity pool, the store that holds every
/// transaction carrying validity predicates.
///
/// Each field falls back to the protocol pool's pending-subpool limit (or
/// account-slot limit) when unset, so by default the validity pool is as large
/// as the pending pool it was carved out of. Standard transactions and
/// validity transactions can then never evict one another: each side has its
/// own budget.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ValidityPoolLimits {
    /// Maximum number of validity transactions kept at once.
    pub max_txs: Option<usize>,
    /// Maximum summed `size()` of validity transactions kept at once, in bytes.
    pub max_size: Option<usize>,
    /// Maximum validity transactions per sender lane.
    pub max_account_slots: Option<usize>,
}

impl ValidityPoolLimits {
    /// Limits that copy the protocol pool's pending limit in every dimension.
    pub const fn inherit() -> Self {
        Self { max_txs: None, max_size: None, max_account_slots: None }
    }

    /// Caps the number of validity transactions.
    #[must_use]
    pub const fn with_max_txs(mut self, max_txs: usize) -> Self {
        self.max_txs = Some(max_txs);
        self
    }

    /// Caps the summed size of validity transactions, in bytes.
    #[must_use]
    pub const fn with_max_size(mut self, max_size: usize) -> Self {
        self.max_size = Some(max_size);
        self
    }

    /// Caps validity transactions per sender lane.
    #[must_use]
    pub const fn with_max_account_slots(mut self, max_account_slots: usize) -> Self {
        self.max_account_slots = Some(max_account_slots);
        self
    }

    /// Resolves the limits against `config`, filling unset fields from its
    /// pending subpool and account-slot limits.
    pub(crate) fn resolve(self, config: &PoolConfig) -> SidecarLimits {
        let pending = &config.pending_limit;
        let capacity = SubPoolLimit {
            max_txs: self.max_txs.unwrap_or(pending.max_txs),
            max_size: self.max_size.unwrap_or(pending.max_size),
        };
        let mut limits = SidecarLimits::from_pool_config(config);
        limits.capacity = capacity;
        limits.max_account_slots = self.max_account_slots.unwrap_or(config.max_account_slots);
        limits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_fields_copy_the_pending_limit() {
        let config = PoolConfig {
            pending_limit: SubPoolLimit { max_txs: 123, max_size: 456 },
            max_account_slots: 7,
            ..PoolConfig::default()
        };
        let limits = ValidityPoolLimits::inherit().resolve(&config);
        assert_eq!(limits.capacity, SubPoolLimit { max_txs: 123, max_size: 456 });
        assert_eq!(limits.max_account_slots, 7);
    }

    #[test]
    fn explicit_fields_override_the_pending_limit() {
        let config = PoolConfig::default();
        let limits = ValidityPoolLimits::inherit()
            .with_max_txs(5)
            .with_max_size(6)
            .with_max_account_slots(8)
            .resolve(&config);
        assert_eq!(limits.capacity, SubPoolLimit { max_txs: 5, max_size: 6 });
        assert_eq!(limits.max_account_slots, 8);
    }
}
