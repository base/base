use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::SolEvent;
use base_precompile_storage::{BasePrecompileError, Result};

use crate::{IB20, Token, TokenAccounting};

/// Token minting operations.
///
/// All methods have default implementations that go through [`Token::accounting`].
/// Implement this trait with an empty body to opt in.
pub trait Mintable: Token {
    /// Creates `amount` tokens at `to`. Enforces supply cap. Emits `Transfer(0x0, to, amount)`.
    fn mint(&mut self, to: Address, amount: U256) -> Result<()> {
        if to == Address::ZERO {
            return Err(BasePrecompileError::revert(IB20::InvalidReceiver { receiver: to }));
        }
        let supply = self.accounting().total_supply()?;
        let cap = self.accounting().supply_cap()?;
        let new_supply =
            supply.checked_add(amount).ok_or_else(BasePrecompileError::under_overflow)?;
        if new_supply > cap {
            return Err(BasePrecompileError::revert(IB20::SupplyCapExceeded {
                cap,
                attempted: new_supply,
            }));
        }
        self.accounting_mut().set_total_supply(new_supply)?;
        let to_balance = self.accounting().balance_of(to)?;
        let new_balance =
            to_balance.checked_add(amount).ok_or_else(BasePrecompileError::under_overflow)?;
        self.accounting_mut().set_balance(to, new_balance)?;
        self.accounting_mut()
            .emit_event(IB20::Transfer { from: Address::ZERO, to, amount }.encode_log_data())
    }

    /// [`Self::mint`] followed by a `Memo` event.
    fn mint_with_memo(&mut self, to: Address, amount: U256, memo: B256) -> Result<()> {
        self.mint(to, amount)?;
        self.accounting_mut().emit_event(IB20::Memo { memo }.encode_log_data())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};
    use rstest::rstest;

    use super::Mintable;
    use crate::common::{
        Token, TokenAccounting,
        test_utils::{InMemoryPolicy, InMemoryTokenAccounting, TestToken},
    };

    const ALICE: Address = Address::repeat_byte(0xaa);

    fn make_token() -> TestToken {
        TestToken::with_storage_and_policy(
            InMemoryTokenAccounting::new(Address::repeat_byte(1)),
            InMemoryPolicy::new(),
        )
    }

    #[test]
    fn mint_increases_balance_and_total_supply() {
        let mut token = make_token();
        token.mint(ALICE, U256::from(100u64)).unwrap();

        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(100u64));
        assert_eq!(token.accounting().total_supply().unwrap(), U256::from(100u64));
        assert_eq!(token.accounting().events.len(), 1);
    }

    #[test]
    fn mint_to_zero_address_reverts() {
        let mut token = make_token();
        assert!(token.mint(Address::ZERO, U256::from(1u64)).is_err());
    }

    #[rstest]
    #[case::at_cap(100u64, 100u64, true)]
    #[case::exceeds_cap(50u64, 51u64, false)]
    fn mint_respects_supply_cap(#[case] cap: u64, #[case] amount: u64, #[case] succeeds: bool) {
        let mut token = make_token();
        token.accounting_mut().supply_cap = U256::from(cap);
        assert_eq!(token.mint(ALICE, U256::from(amount)).is_ok(), succeeds);
    }

    #[test]
    fn mint_accumulates_across_calls() {
        let mut token = make_token();
        token.mint(ALICE, U256::from(40u64)).unwrap();
        token.mint(ALICE, U256::from(60u64)).unwrap();

        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(100u64));
        assert_eq!(token.accounting().total_supply().unwrap(), U256::from(100u64));
    }
}
