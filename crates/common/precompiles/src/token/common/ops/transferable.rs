use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::SolEvent;
use base_precompile_storage::{BasePrecompileError, Result};

use crate::token::{
    IDefaultToken,
    common::{Token, TokenAccounting},
};

/// ERC-20 transfer, approval, and memo-decorated transfer operations.
///
/// All methods have default implementations that go through [`Token::accounting`].
/// Implement this trait with an empty body to opt in.
pub trait Transferable: Token {
    /// Moves `amount` tokens from `from` to `to`. Emits `Transfer`.
    fn transfer(&mut self, from: Address, to: Address, amount: U256) -> Result<()> {
        if from == Address::ZERO {
            return Err(BasePrecompileError::revert(IDefaultToken::InvalidSender { sender: from }));
        }
        if to == Address::ZERO {
            return Err(BasePrecompileError::revert(IDefaultToken::InvalidReceiver {
                receiver: to,
            }));
        }
        let from_balance = self.accounting().balance_of(from)?;
        if from_balance < amount {
            return Err(BasePrecompileError::revert(IDefaultToken::InsufficientBalance {
                sender: from,
                balance: from_balance,
                needed: amount,
            }));
        }
        self.accounting_mut().set_balance(from, from_balance - amount)?;
        let to_balance = self.accounting().balance_of(to)?;
        let new_to_balance =
            to_balance.checked_add(amount).ok_or_else(BasePrecompileError::under_overflow)?;
        self.accounting_mut().set_balance(to, new_to_balance)?;
        self.accounting_mut()
            .emit_event(IDefaultToken::Transfer { from, to, amount }.encode_log_data())
    }

    /// Moves `amount` tokens from `from` to `to` using `spender`'s allowance.
    /// Emits `Transfer`. Skips allowance decrement when allowance is `U256::MAX`.
    fn transfer_from(
        &mut self,
        spender: Address,
        from: Address,
        to: Address,
        amount: U256,
    ) -> Result<()> {
        if from == Address::ZERO {
            return Err(BasePrecompileError::revert(IDefaultToken::InvalidSender { sender: from }));
        }
        let allowance = self.accounting().allowance(from, spender)?;
        if allowance != U256::MAX {
            if allowance < amount {
                return Err(BasePrecompileError::revert(IDefaultToken::InsufficientAllowance {
                    spender,
                    allowance,
                    needed: amount,
                }));
            }
            self.transfer(from, to, amount)?;
            self.accounting_mut().set_allowance(from, spender, allowance - amount)
        } else {
            self.transfer(from, to, amount)
        }
    }

    /// Sets `spender`'s allowance from `owner` to `amount`. Emits `Approval`.
    fn approve(&mut self, owner: Address, spender: Address, amount: U256) -> Result<()> {
        if owner == Address::ZERO {
            return Err(BasePrecompileError::revert(IDefaultToken::InvalidApprover {
                approver: owner,
            }));
        }
        if spender == Address::ZERO {
            return Err(BasePrecompileError::revert(IDefaultToken::InvalidSpender { spender }));
        }
        self.accounting_mut().set_allowance(owner, spender, amount)?;
        self.accounting_mut()
            .emit_event(IDefaultToken::Approval { owner, spender, amount }.encode_log_data())
    }

    /// [`Self::transfer`] followed by a `Memo` event.
    fn transfer_with_memo(
        &mut self,
        from: Address,
        to: Address,
        amount: U256,
        memo: B256,
    ) -> Result<()> {
        self.transfer(from, to, amount)?;
        self.accounting_mut().emit_event(IDefaultToken::Memo { memo }.encode_log_data())
    }

    /// [`Self::transfer_from`] followed by a `Memo` event.
    fn transfer_from_with_memo(
        &mut self,
        spender: Address,
        from: Address,
        to: Address,
        amount: U256,
        memo: B256,
    ) -> Result<()> {
        self.transfer_from(spender, from, to, amount)?;
        self.accounting_mut().emit_event(IDefaultToken::Memo { memo }.encode_log_data())
    }
}
