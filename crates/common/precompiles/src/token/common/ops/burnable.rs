use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::SolEvent;
use base_precompile_storage::{BasePrecompileError, Result};

use crate::token::{
    IB20,
    common::{Token, TokenAccounting},
};

/// Token burn operations.
///
/// All methods have default implementations that go through [`Token::accounting`].
/// Implement this trait with an empty body to opt in.
pub trait Burnable: Token {
    /// Destroys `amount` tokens from `from`. Emits `Transfer(from, 0x0, amount)`.
    fn burn(&mut self, from: Address, amount: U256) -> Result<()> {
        let balance = self.accounting().balance_of(from)?;
        if balance < amount {
            return Err(BasePrecompileError::revert(IB20::InsufficientBalance {
                sender: from,
                balance,
                needed: amount,
            }));
        }
        self.accounting_mut().set_balance(from, balance - amount)?;
        let supply = self.accounting().total_supply()?;
        let new_supply =
            supply.checked_sub(amount).ok_or_else(BasePrecompileError::under_overflow)?;
        self.accounting_mut().set_total_supply(new_supply)?;
        self.accounting_mut()
            .emit_event(IB20::Transfer { from, to: Address::ZERO, amount }.encode_log_data())
    }

    /// [`Self::burn`] followed by a `Memo` event.
    fn burn_with_memo(&mut self, from: Address, amount: U256, memo: B256) -> Result<()> {
        self.burn(from, amount)?;
        self.accounting_mut().emit_event(IB20::Memo { memo }.encode_log_data())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};

    use crate::token::common::{
        Token, TokenAccounting,
        test_utils::{InMemoryPolicy, InMemoryTokenAccounting, TestToken},
    };

    use super::Burnable;

    const ALICE: Address = Address::repeat_byte(0xaa);

    fn make_token() -> TestToken {
        TestToken::with_storage_and_policy(
            InMemoryTokenAccounting::new(Address::repeat_byte(1)),
            InMemoryPolicy::new(),
        )
    }

    #[test]
    fn burn_decreases_balance_and_total_supply() {
        let mut token = make_token();
        token.accounting_mut().balances.insert(ALICE, U256::from(100u64));
        token.accounting_mut().total_supply = U256::from(100u64);

        token.burn(ALICE, U256::from(40u64)).unwrap();

        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(60u64));
        assert_eq!(token.accounting().total_supply().unwrap(), U256::from(60u64));
        assert_eq!(token.accounting().events.len(), 1);
    }

    #[test]
    fn burn_insufficient_balance_reverts() {
        let mut token = make_token();
        token.accounting_mut().balances.insert(ALICE, U256::from(10u64));
        token.accounting_mut().total_supply = U256::from(10u64);
        assert!(token.burn(ALICE, U256::from(11u64)).is_err());
    }

    #[test]
    fn burn_full_balance_zeroes_supply() {
        let mut token = make_token();
        token.accounting_mut().balances.insert(ALICE, U256::from(50u64));
        token.accounting_mut().total_supply = U256::from(50u64);

        token.burn(ALICE, U256::from(50u64)).unwrap();

        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::ZERO);
        assert_eq!(token.accounting().total_supply().unwrap(), U256::ZERO);
    }
}
