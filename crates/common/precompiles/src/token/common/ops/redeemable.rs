use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::SolEvent;
use base_precompile_storage::{BasePrecompileError, Result};

use super::Burnable;
use crate::token::{IDefaultToken, common::TokenAccounting};

/// User-initiated redeem (burn with off-chain settlement implication) and related admin.
///
/// Requires [`Burnable`] since `redeem` internally calls [`Burnable::burn`].
/// All methods have default implementations. Implement with an empty body to opt in.
pub trait Redeemable: Burnable {
    /// Burns `amount` from `caller`. Enforces minimum. Emits `Transfer` then `Redeemed`.
    fn redeem(&mut self, caller: Address, amount: U256) -> Result<()> {
        let minimum = self.accounting().minimum_redeemable()?;
        if amount < minimum {
            return Err(BasePrecompileError::revert(IDefaultToken::MinimumRedeemableNotMet {
                amount,
                minimum,
            }));
        }
        self.burn(caller, amount)?;
        self.accounting_mut()
            .emit_event(IDefaultToken::Redeemed { holder: caller, amount }.encode_log_data())
    }

    /// [`Self::redeem`] followed by a `Memo` event.
    fn redeem_with_memo(&mut self, caller: Address, amount: U256, memo: B256) -> Result<()> {
        self.redeem(caller, amount)?;
        self.accounting_mut().emit_event(IDefaultToken::Memo { memo }.encode_log_data())
    }

    /// Updates the minimum redeemable amount. Emits `MinimumRedeemableUpdated`.
    fn set_minimum_redeemable(&mut self, caller: Address, minimum: U256) -> Result<()> {
        let old = self.accounting().minimum_redeemable()?;
        self.accounting_mut().set_minimum_redeemable(minimum)?;
        self.accounting_mut().emit_event(
            IDefaultToken::MinimumRedeemableUpdated {
                updater: caller,
                oldMinimum: old,
                newMinimum: minimum,
            }
            .encode_log_data(),
        )
    }
}
