use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::SolEvent;
use base_precompile_storage::{BasePrecompileError, Result};

use super::Burnable;
use crate::{IB20, TokenAccounting};

/// User-initiated redeem (burn with off-chain settlement implication) and related admin.
///
/// Requires [`Burnable`] since `redeem` internally calls [`Burnable::burn`].
/// All methods have default implementations. Implement with an empty body to opt in.
pub trait Redeemable: Burnable {
    /// Burns `amount` from `caller`. Enforces minimum. Emits `Transfer` then `Redeemed`.
    fn redeem(&mut self, caller: Address, amount: U256) -> Result<()> {
        let minimum = self.accounting().minimum_redeemable()?;
        if amount < minimum {
            return Err(BasePrecompileError::revert(IB20::MinimumRedeemableNotMet {
                amount,
                minimum,
            }));
        }
        self.burn(caller, amount)?;
        self.accounting_mut()
            .emit_event(IB20::Redeemed { holder: caller, amount }.encode_log_data())
    }

    /// [`Self::redeem`] followed by a `Memo` event.
    fn redeem_with_memo(&mut self, caller: Address, amount: U256, memo: B256) -> Result<()> {
        self.redeem(caller, amount)?;
        self.accounting_mut().emit_event(IB20::Memo { memo }.encode_log_data())
    }

    /// Updates the minimum redeemable amount. Emits `MinimumRedeemableUpdated`.
    fn set_minimum_redeemable(&mut self, caller: Address, minimum: U256) -> Result<()> {
        let old = self.accounting().minimum_redeemable()?;
        self.accounting_mut().set_minimum_redeemable(minimum)?;
        self.accounting_mut().emit_event(
            IB20::MinimumRedeemableUpdated {
                updater: caller,
                oldMinimum: old,
                newMinimum: minimum,
            }
            .encode_log_data(),
        )
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};
    use rstest::rstest;

    use super::Redeemable;
    use crate::common::{
        Token, TokenAccounting,
        test_utils::{InMemoryPolicy, InMemoryTokenAccounting, TestToken},
    };

    const CALLER: Address = Address::repeat_byte(0xaa);

    fn make_token() -> TestToken {
        TestToken::with_storage_and_policy(
            InMemoryTokenAccounting::new(Address::repeat_byte(1)),
            InMemoryPolicy::new(),
        )
    }

    #[test]
    fn redeem_burns_balance_and_emits_transfer_and_redeemed() {
        let mut token = make_token();
        token.accounting_mut().balances.insert(CALLER, U256::from(100u64));
        token.accounting_mut().total_supply = U256::from(100u64);

        token.redeem(CALLER, U256::from(50u64)).unwrap();

        assert_eq!(token.accounting().balance_of(CALLER).unwrap(), U256::from(50u64));
        assert_eq!(token.accounting().total_supply().unwrap(), U256::from(50u64));
        assert_eq!(token.accounting().events.len(), 2);
    }

    #[rstest]
    #[case::below_minimum(5u64, false)]
    #[case::at_minimum(10u64, true)]
    fn redeem_enforces_minimum(#[case] amount: u64, #[case] succeeds: bool) {
        let mut token = make_token();
        token.accounting_mut().balances.insert(CALLER, U256::from(100u64));
        token.accounting_mut().total_supply = U256::from(100u64);
        token.accounting_mut().minimum_redeemable = U256::from(10u64);
        assert_eq!(token.redeem(CALLER, U256::from(amount)).is_ok(), succeeds);
    }

    #[test]
    fn redeem_insufficient_balance_reverts() {
        let mut token = make_token();
        token.accounting_mut().balances.insert(CALLER, U256::from(5u64));
        token.accounting_mut().total_supply = U256::from(5u64);

        assert!(token.redeem(CALLER, U256::from(10u64)).is_err());
    }

    #[test]
    fn set_minimum_redeemable_updates_and_emits_event() {
        let mut token = make_token();
        token.set_minimum_redeemable(CALLER, U256::from(25u64)).unwrap();

        assert_eq!(token.accounting().minimum_redeemable().unwrap(), U256::from(25u64));
        assert_eq!(token.accounting().events.len(), 1);
    }
}
