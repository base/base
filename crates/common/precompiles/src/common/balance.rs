//! B-20 balance constraints.

use alloy_primitives::{Address, U256};
use base_precompile_storage::{BasePrecompileError, Result};

use crate::IB20;

/// Balance constraints for B-20 token accounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct B20Balance;

impl B20Balance {
    /// Maximum balance for a single account.
    ///
    /// The upper 128 bits of the storage slot are reserved for future per-account
    /// accounting extensions such as locked and unlocked balances.
    pub const MAX: U256 = U256::from_limbs([u64::MAX, u64::MAX, 0, 0]);

    /// Adds `amount` to `balance`, rejecting values that exceed [`Self::MAX`].
    pub fn checked_add(account: Address, balance: U256, amount: U256) -> Result<U256> {
        let attempted = balance.saturating_add(amount);
        if attempted > Self::MAX {
            return Err(BasePrecompileError::revert(IB20::MaxBalanceExceeded {
                account,
                maxBalance: Self::MAX,
                attemptedBalance: attempted,
            }));
        }
        Ok(attempted)
    }
}
