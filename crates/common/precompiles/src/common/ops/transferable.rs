use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::SolEvent;
use base_precompile_storage::{BasePrecompileError, Result};

use crate::{IB20, Token, TokenAccounting};

/// ERC-20 transfer, approval, and memo-decorated transfer operations.
///
/// All methods have default implementations that go through [`Token::accounting`].
/// Implement this trait with an empty body to opt in.
pub trait Transferable: Token {
    /// Moves `amount` tokens from `from` to `to`. Emits `Transfer`.
    fn transfer(&mut self, from: Address, to: Address, amount: U256) -> Result<()> {
        if from == Address::ZERO {
            return Err(BasePrecompileError::revert(IB20::InvalidSender { sender: from }));
        }
        if to == Address::ZERO {
            return Err(BasePrecompileError::revert(IB20::InvalidReceiver { receiver: to }));
        }
        let from_balance = self.accounting().balance_of(from)?;
        if from_balance < amount {
            return Err(BasePrecompileError::revert(IB20::InsufficientBalance {
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
        self.accounting_mut().emit_event(IB20::Transfer { from, to, amount }.encode_log_data())
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
            return Err(BasePrecompileError::revert(IB20::InvalidSender { sender: from }));
        }
        let allowance = self.accounting().allowance(from, spender)?;
        if allowance != U256::MAX {
            if allowance < amount {
                return Err(BasePrecompileError::revert(IB20::InsufficientAllowance {
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
            return Err(BasePrecompileError::revert(IB20::InvalidApprover { approver: owner }));
        }
        if spender == Address::ZERO {
            return Err(BasePrecompileError::revert(IB20::InvalidSpender { spender }));
        }
        self.accounting_mut().set_allowance(owner, spender, amount)?;
        self.accounting_mut()
            .emit_event(IB20::Approval { owner, spender, amount }.encode_log_data())
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
        self.accounting_mut().emit_event(IB20::Memo { memo }.encode_log_data())
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
        self.accounting_mut().emit_event(IB20::Memo { memo }.encode_log_data())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};
    use rstest::rstest;

    use super::Transferable;
    use crate::common::{
        Token, TokenAccounting,
        test_utils::{InMemoryPolicy, InMemoryTokenAccounting, TestToken},
    };

    const ALICE: Address = Address::repeat_byte(0xaa);
    const BOB: Address = Address::repeat_byte(0xbb);
    const SPENDER: Address = Address::repeat_byte(0xcc);

    fn make_token() -> TestToken {
        TestToken::with_storage_and_policy(
            InMemoryTokenAccounting::new(Address::repeat_byte(1)),
            InMemoryPolicy::new(),
        )
    }

    #[test]
    fn transfer_moves_balances_and_emits_event() {
        let mut token = make_token();
        token.accounting_mut().balances.insert(ALICE, U256::from(100u64));

        token.transfer(ALICE, BOB, U256::from(40u64)).unwrap();

        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(60u64));
        assert_eq!(token.accounting().balance_of(BOB).unwrap(), U256::from(40u64));
        assert_eq!(token.accounting().events.len(), 1);
    }

    #[test]
    fn transfer_from_zero_sender_reverts() {
        let mut token = make_token();
        assert!(token.transfer(Address::ZERO, BOB, U256::from(1u64)).is_err());
    }

    #[test]
    fn transfer_to_zero_receiver_reverts() {
        let mut token = make_token();
        token.accounting_mut().balances.insert(ALICE, U256::from(100u64));
        assert!(token.transfer(ALICE, Address::ZERO, U256::from(1u64)).is_err());
    }

    #[test]
    fn transfer_insufficient_balance_reverts() {
        let mut token = make_token();
        token.accounting_mut().balances.insert(ALICE, U256::from(5u64));
        assert!(token.transfer(ALICE, BOB, U256::from(10u64)).is_err());
    }

    #[test]
    fn approve_sets_allowance_and_emits_event() {
        let mut token = make_token();
        token.approve(ALICE, SPENDER, U256::from(50u64)).unwrap();

        assert_eq!(token.accounting().allowance(ALICE, SPENDER).unwrap(), U256::from(50u64));
        assert_eq!(token.accounting().events.len(), 1);
    }

    #[rstest]
    #[case::finite(U256::from(30u64), U256::from(20u64), Some(U256::from(10u64)))]
    #[case::max_allowance(U256::MAX, U256::from(50u64), Some(U256::MAX))]
    #[case::insufficient(U256::from(5u64), U256::from(10u64), None)]
    fn transfer_from_allowance_cases(
        #[case] allowance: U256,
        #[case] amount: U256,
        #[case] expected_remaining: Option<U256>,
    ) {
        let mut token = make_token();
        token.accounting_mut().balances.insert(ALICE, U256::from(100u64));
        token.accounting_mut().allowances.insert((ALICE, SPENDER), allowance);
        let result = token.transfer_from(SPENDER, ALICE, BOB, amount);
        match expected_remaining {
            Some(rem) => {
                result.unwrap();
                assert_eq!(token.accounting().allowance(ALICE, SPENDER).unwrap(), rem);
            }
            None => assert!(result.is_err()),
        }
    }
}
