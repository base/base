use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::SolEvent;
use base_precompile_storage::{BasePrecompileError, Result};

use crate::token::{
    IDefaultToken,
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
            return Err(BasePrecompileError::revert(IDefaultToken::InsufficientBalance {
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
        self.accounting_mut().emit_event(
            IDefaultToken::Transfer { from, to: Address::ZERO, amount }.encode_log_data(),
        )
    }

    /// [`Self::burn`] followed by a `Memo` event.
    fn burn_with_memo(&mut self, from: Address, amount: U256, memo: B256) -> Result<()> {
        self.burn(from, amount)?;
        self.accounting_mut().emit_event(IDefaultToken::Memo { memo }.encode_log_data())
    }
}
