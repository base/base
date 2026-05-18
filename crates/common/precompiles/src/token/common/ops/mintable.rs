use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::SolEvent;
use base_precompile_storage::{BasePrecompileError, Result};

use crate::token::{
    IDefaultToken,
    common::{Token, TokenAccounting},
};

/// Token minting operations.
///
/// All methods have default implementations that go through [`Token::accounting`].
/// Implement this trait with an empty body to opt in.
pub trait Mintable: Token {
    /// Creates `amount` tokens at `to`. Enforces supply cap. Emits `Transfer(0x0, to, amount)`.
    fn mint(&mut self, to: Address, amount: U256) -> Result<()> {
        if to == Address::ZERO {
            return Err(BasePrecompileError::revert(IDefaultToken::InvalidReceiver {
                receiver: to,
            }));
        }
        let supply = self.accounting().total_supply()?;
        let cap = self.accounting().supply_cap()?;
        let new_supply =
            supply.checked_add(amount).ok_or_else(BasePrecompileError::under_overflow)?;
        if new_supply > cap {
            return Err(BasePrecompileError::revert(IDefaultToken::SupplyCapExceeded {
                cap,
                attempted: new_supply,
            }));
        }
        self.accounting_mut().set_total_supply(new_supply)?;
        let to_balance = self.accounting().balance_of(to)?;
        let new_balance =
            to_balance.checked_add(amount).ok_or_else(BasePrecompileError::under_overflow)?;
        self.accounting_mut().set_balance(to, new_balance)?;
        self.accounting_mut().emit_event(
            IDefaultToken::Transfer { from: Address::ZERO, to, amount }.encode_log_data(),
        )
    }

    /// [`Self::mint`] followed by a `Memo` event.
    fn mint_with_memo(&mut self, to: Address, amount: U256, memo: B256) -> Result<()> {
        self.mint(to, amount)?;
        self.accounting_mut().emit_event(IDefaultToken::Memo { memo }.encode_log_data())
    }
}
