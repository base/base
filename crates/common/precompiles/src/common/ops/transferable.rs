use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::SolEvent;
use base_precompile_storage::{BasePrecompileError, Result};

use super::guards::B20Guards;
use crate::{B20PolicyType, IB20, Token, TokenAccounting};

/// ERC-20 transfer, approval, and memo-decorated transfer operations.
///
/// All methods have default implementations that go through [`Token::accounting`].
/// Implement this trait with an empty body to opt in.
pub trait Transferable: Token {
    /// Moves `amount` tokens from `from` to `to`. Emits `Transfer`.
    ///
    /// When `privileged` is true (factory bootstrap window) the pause and
    /// policy checks are skipped; balance invariants are always enforced.
    fn transfer(
        &mut self,
        from: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()> {
        if from == Address::ZERO {
            return Err(BasePrecompileError::revert(IB20::InvalidSender { sender: from }));
        }
        if to == Address::ZERO {
            return Err(BasePrecompileError::revert(IB20::InvalidReceiver { receiver: to }));
        }
        if !privileged {
            B20Guards::ensure_not_paused::<Self>(self, IB20::PausableFeature::TRANSFER)?;
            B20Guards::ensure_policy_type::<Self>(self, B20PolicyType::TransferSender, from)?;
            B20Guards::ensure_policy_type::<Self>(self, B20PolicyType::TransferReceiver, to)?;
        }
        let from_balance = self.accounting().balance_of(from)?;
        if from_balance < amount {
            return Err(BasePrecompileError::revert(IB20::InsufficientBalance {
                sender: from,
                balance: from_balance,
                needed: amount,
            }));
        }
        let new_from_balance =
            from_balance.checked_sub(amount).ok_or_else(BasePrecompileError::under_overflow)?;
        self.accounting_mut().set_balance(from, new_from_balance)?;
        let to_balance = self.accounting().balance_of(to)?;
        let new_to_balance =
            to_balance.checked_add(amount).ok_or_else(BasePrecompileError::under_overflow)?;
        self.accounting_mut().set_balance(to, new_to_balance)?;
        self.accounting_mut().emit_event(IB20::Transfer { from, to, amount }.encode_log_data())
    }

    /// Moves `amount` tokens from `from` to `to` using `spender`'s allowance.
    /// Emits `Transfer`. Skips allowance decrement when allowance is `U256::MAX`.
    ///
    /// When `privileged` is true the executor policy check is skipped; the
    /// inner `transfer` call also receives `privileged`.
    fn transfer_from(
        &mut self,
        spender: Address,
        from: Address,
        to: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()> {
        if from == Address::ZERO {
            return Err(BasePrecompileError::revert(IB20::InvalidSender { sender: from }));
        }
        if !privileged && spender != from {
            B20Guards::ensure_policy_type::<Self>(self, B20PolicyType::TransferExecutor, spender)?;
        }
        let allowance = self.accounting().allowance(from, spender)?;
        if allowance == U256::MAX {
            return self.transfer(from, to, amount, privileged);
        }
        if allowance < amount {
            return Err(BasePrecompileError::revert(IB20::InsufficientAllowance {
                spender,
                allowance,
                needed: amount,
            }));
        }
        self.transfer(from, to, amount, privileged)?;
        self.accounting_mut().set_allowance(from, spender, allowance - amount)
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
        privileged: bool,
    ) -> Result<()> {
        self.transfer(from, to, amount, privileged)?;
        self.accounting_mut().emit_event(IB20::Memo { caller: from, memo }.encode_log_data())
    }

    /// [`Self::transfer_from`] followed by a `Memo` event.
    fn transfer_from_with_memo(
        &mut self,
        spender: Address,
        from: Address,
        to: Address,
        amount: U256,
        memo: B256,
        privileged: bool,
    ) -> Result<()> {
        self.transfer_from(spender, from, to, amount, privileged)?;
        self.accounting_mut().emit_event(IB20::Memo { caller: spender, memo }.encode_log_data())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, U256};
    use alloy_sol_types::SolEvent;
    use base_precompile_storage::BasePrecompileError;

    use super::Transferable;
    use crate::{
        B20PausableFeature, B20PolicyType, IB20, PolicyRegistryStorage,
        common::{
            Token, TokenAccounting,
            test_utils::{InMemoryPolicy, InMemoryTokenAccounting, TestToken},
        },
    };

    const ALICE: Address = Address::repeat_byte(0xaa);
    const BOB: Address = Address::repeat_byte(0xbb);
    const SPENDER: Address = Address::repeat_byte(0xcc);
    const TOKEN_ADDR: Address = Address::repeat_byte(1);

    fn make_token() -> TestToken {
        TestToken::with_storage_and_policy(
            InMemoryTokenAccounting::new(TOKEN_ADDR),
            InMemoryPolicy::new(),
        )
    }

    fn token_with_balance(balance: U256) -> TestToken {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN_ADDR);
        accounting.balances.insert(ALICE, balance);
        TestToken::with_storage_and_policy(accounting, InMemoryPolicy::new())
    }

    #[test]
    fn transfer_moves_balances_and_emits_event() {
        let mut token = token_with_balance(U256::from(100u64));

        token.transfer(ALICE, BOB, U256::from(40u64), false).unwrap();

        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(60u64));
        assert_eq!(token.accounting().balance_of(BOB).unwrap(), U256::from(40u64));
        assert_eq!(token.accounting().events.len(), 1);
    }

    #[test]
    fn transfer_to_self_preserves_balance_and_emits_event() {
        let mut token = token_with_balance(U256::from(100u64));

        token.transfer(ALICE, ALICE, U256::from(30u64), false).unwrap();

        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(100u64));
        assert_eq!(
            token.accounting().events[0],
            IB20::Transfer { from: ALICE, to: ALICE, amount: U256::from(30u64) }.encode_log_data()
        );
    }

    /// Regression: self-transfers must not mint tokens.
    ///
    /// A naive two-write transfer computes `new_from = balance - amount` and
    /// `new_to = balance + amount` from the same pre-debit read, then writes both
    /// to the same slot when `from == to`. The second write wins at `balance + amount`,
    /// inflating supply by `amount` on every self-transfer.
    #[test]
    fn transfer_to_self_repeated_calls_do_not_inflate_balance() {
        let initial = U256::from(100u64);
        let amount = U256::from(50u64);
        let mut token = token_with_balance(initial);

        for _ in 0..5 {
            token.transfer(ALICE, ALICE, amount, false).unwrap();
        }

        assert_eq!(
            token.accounting().balance_of(ALICE).unwrap(),
            initial,
            "each self-transfer must leave balance unchanged; a buggy dual absolute write would mint 50 tokens per call"
        );
        assert_eq!(token.accounting().events.len(), 5);
    }

    #[test]
    fn transfer_from_zero_sender_reverts() {
        let mut token = make_token();

        assert_eq!(
            token.transfer(Address::ZERO, BOB, U256::ONE, false).unwrap_err(),
            BasePrecompileError::revert(IB20::InvalidSender { sender: Address::ZERO })
        );
    }

    #[test]
    fn transfer_to_zero_receiver_reverts() {
        let mut token = token_with_balance(U256::from(100u64));

        assert_eq!(
            token.transfer(ALICE, Address::ZERO, U256::ONE, false).unwrap_err(),
            BasePrecompileError::revert(IB20::InvalidReceiver { receiver: Address::ZERO })
        );
    }

    #[test]
    fn transfer_insufficient_balance_reverts() {
        let mut token = token_with_balance(U256::from(5u64));

        assert_eq!(
            token.transfer(ALICE, BOB, U256::from(10u64), false).unwrap_err(),
            BasePrecompileError::revert(IB20::InsufficientBalance {
                sender: ALICE,
                balance: U256::from(5u64),
                needed: U256::from(10u64),
            })
        );
    }

    #[test]
    fn approve_sets_allowance_and_emits_event() {
        let mut token = make_token();

        token.approve(ALICE, SPENDER, U256::from(50u64)).unwrap();

        assert_eq!(token.accounting().allowance(ALICE, SPENDER).unwrap(), U256::from(50u64));
        assert_eq!(token.accounting().events.len(), 1);
    }

    #[test]
    fn approve_from_zero_owner_reverts() {
        let mut token = make_token();

        assert_eq!(
            token.approve(Address::ZERO, SPENDER, U256::ONE).unwrap_err(),
            BasePrecompileError::revert(IB20::InvalidApprover { approver: Address::ZERO })
        );
    }

    #[test]
    fn approve_to_zero_spender_reverts() {
        let mut token = make_token();

        assert_eq!(
            token.approve(ALICE, Address::ZERO, U256::ONE).unwrap_err(),
            BasePrecompileError::revert(IB20::InvalidSpender { spender: Address::ZERO })
        );
    }

    #[test]
    fn transfer_from_with_finite_allowance_decrements_allowance() {
        let mut token = token_with_balance(U256::from(100u64));
        token.accounting_mut().allowances.insert((ALICE, SPENDER), U256::from(30u64));

        token.transfer_from(SPENDER, ALICE, BOB, U256::from(20u64), false).unwrap();

        assert_eq!(token.accounting().allowance(ALICE, SPENDER).unwrap(), U256::from(10u64));
        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(80u64));
        assert_eq!(token.accounting().balance_of(BOB).unwrap(), U256::from(20u64));
    }

    #[test]
    fn transfer_from_with_max_allowance_preserves_allowance() {
        let mut token = token_with_balance(U256::from(100u64));
        token.accounting_mut().allowances.insert((ALICE, SPENDER), U256::MAX);

        token.transfer_from(SPENDER, ALICE, BOB, U256::from(20u64), false).unwrap();

        assert_eq!(token.accounting().allowance(ALICE, SPENDER).unwrap(), U256::MAX);
        assert_eq!(token.accounting().balance_of(BOB).unwrap(), U256::from(20u64));
    }

    #[test]
    fn transfer_from_with_insufficient_allowance_reverts() {
        let mut token = token_with_balance(U256::from(100u64));
        token.accounting_mut().allowances.insert((ALICE, SPENDER), U256::from(5u64));

        assert_eq!(
            token.transfer_from(SPENDER, ALICE, BOB, U256::from(10u64), false).unwrap_err(),
            BasePrecompileError::revert(IB20::InsufficientAllowance {
                spender: SPENDER,
                allowance: U256::from(5u64),
                needed: U256::from(10u64),
            })
        );
    }

    #[test]
    fn transfer_with_memo_emits_transfer_and_memo() {
        let mut token = token_with_balance(U256::from(100u64));

        token
            .transfer_with_memo(ALICE, BOB, U256::from(10u64), B256::repeat_byte(0x42), false)
            .unwrap();

        assert_eq!(token.accounting().events.len(), 2);
    }

    #[test]
    fn transfer_reverts_when_transfer_feature_paused() {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN_ADDR);
        accounting.balances.insert(ALICE, U256::from(10u64));
        accounting.paused = B20PausableFeature::mask(IB20::PausableFeature::TRANSFER);
        let mut token = TestToken::with_storage_and_policy(accounting, InMemoryPolicy::new());

        assert_eq!(
            token.transfer(ALICE, BOB, U256::ONE, false).unwrap_err(),
            BasePrecompileError::revert(IB20::ContractPaused {
                feature: IB20::PausableFeature::TRANSFER,
            })
        );
    }

    #[test]
    fn transfer_reverts_when_sender_policy_denies() {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN_ADDR);
        accounting.balances.insert(ALICE, U256::from(10u64));
        accounting
            .policy_ids
            .insert(B20PolicyType::TransferSender.id(), PolicyRegistryStorage::ALWAYS_BLOCK_ID);
        let mut token = TestToken::with_storage_and_policy(accounting, InMemoryPolicy::new());

        assert_eq!(
            token.transfer(ALICE, BOB, U256::ONE, false).unwrap_err(),
            BasePrecompileError::revert(IB20::PolicyForbids {
                policyScope: B20PolicyType::TransferSender.id(),
                policyId: PolicyRegistryStorage::ALWAYS_BLOCK_ID,
            })
        );
    }

    #[test]
    fn transfer_reverts_when_receiver_policy_denies() {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN_ADDR);
        accounting.balances.insert(ALICE, U256::from(10u64));
        accounting
            .policy_ids
            .insert(B20PolicyType::TransferReceiver.id(), PolicyRegistryStorage::ALWAYS_BLOCK_ID);
        let mut token = TestToken::with_storage_and_policy(accounting, InMemoryPolicy::new());

        assert_eq!(
            token.transfer(ALICE, BOB, U256::ONE, false).unwrap_err(),
            BasePrecompileError::revert(IB20::PolicyForbids {
                policyScope: B20PolicyType::TransferReceiver.id(),
                policyId: PolicyRegistryStorage::ALWAYS_BLOCK_ID,
            })
        );
    }

    #[test]
    fn transfer_from_reverts_when_executor_policy_denies() {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN_ADDR);
        accounting.balances.insert(ALICE, U256::from(10u64));
        accounting.allowances.insert((ALICE, SPENDER), U256::from(10u64));
        accounting
            .policy_ids
            .insert(B20PolicyType::TransferExecutor.id(), PolicyRegistryStorage::ALWAYS_BLOCK_ID);
        let mut token = TestToken::with_storage_and_policy(accounting, InMemoryPolicy::new());

        assert_eq!(
            token.transfer_from(SPENDER, ALICE, BOB, U256::ONE, false).unwrap_err(),
            BasePrecompileError::revert(IB20::PolicyForbids {
                policyScope: B20PolicyType::TransferExecutor.id(),
                policyId: PolicyRegistryStorage::ALWAYS_BLOCK_ID,
            })
        );
    }

    #[test]
    fn transfer_privileged_skips_pause_check() {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN_ADDR);
        accounting.balances.insert(ALICE, U256::from(10u64));
        accounting.paused = B20PausableFeature::mask(IB20::PausableFeature::TRANSFER);
        let mut token = TestToken::with_storage_and_policy(accounting, InMemoryPolicy::new());

        token.transfer(ALICE, BOB, U256::ONE, true).unwrap();

        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(9u64));
        assert_eq!(token.accounting().balance_of(BOB).unwrap(), U256::ONE);
    }

    #[test]
    fn transfer_privileged_skips_sender_policy() {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN_ADDR);
        accounting.balances.insert(ALICE, U256::from(10u64));
        accounting
            .policy_ids
            .insert(B20PolicyType::TransferSender.id(), PolicyRegistryStorage::ALWAYS_BLOCK_ID);
        let mut token = TestToken::with_storage_and_policy(accounting, InMemoryPolicy::new());

        token.transfer(ALICE, BOB, U256::ONE, true).unwrap();

        assert_eq!(token.accounting().balance_of(BOB).unwrap(), U256::ONE);
    }

    #[test]
    fn transfer_privileged_skips_receiver_policy() {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN_ADDR);
        accounting.balances.insert(ALICE, U256::from(10u64));
        accounting
            .policy_ids
            .insert(B20PolicyType::TransferReceiver.id(), PolicyRegistryStorage::ALWAYS_BLOCK_ID);
        let mut token = TestToken::with_storage_and_policy(accounting, InMemoryPolicy::new());

        token.transfer(ALICE, BOB, U256::ONE, true).unwrap();

        assert_eq!(token.accounting().balance_of(BOB).unwrap(), U256::ONE);
    }

    #[test]
    fn transfer_from_privileged_skips_executor_policy() {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN_ADDR);
        accounting.balances.insert(ALICE, U256::from(10u64));
        accounting.allowances.insert((ALICE, SPENDER), U256::from(10u64));
        accounting
            .policy_ids
            .insert(B20PolicyType::TransferExecutor.id(), PolicyRegistryStorage::ALWAYS_BLOCK_ID);
        let mut token = TestToken::with_storage_and_policy(accounting, InMemoryPolicy::new());

        token.transfer_from(SPENDER, ALICE, BOB, U256::ONE, true).unwrap();

        assert_eq!(token.accounting().balance_of(BOB).unwrap(), U256::ONE);
    }

    // ---- Balance checks ----

    #[test]
    fn transfer_exact_balance_succeeds_and_drains_sender() {
        let mut token = token_with_balance(U256::from(50u64));

        token.transfer(ALICE, BOB, U256::from(50u64), false).unwrap();

        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::ZERO);
        assert_eq!(token.accounting().balance_of(BOB).unwrap(), U256::from(50u64));
    }

    #[test]
    fn transfer_from_reverts_when_sender_has_insufficient_balance() {
        let mut token = make_token(); // ALICE has zero balance
        token.accounting_mut().allowances.insert((ALICE, SPENDER), U256::MAX);

        assert_eq!(
            token.transfer_from(SPENDER, ALICE, BOB, U256::ONE, false).unwrap_err(),
            BasePrecompileError::revert(IB20::InsufficientBalance {
                sender: ALICE,
                balance: U256::ZERO,
                needed: U256::ONE,
            })
        );
    }

    // ---- Overflow ----

    #[test]
    fn transfer_reverts_on_receiver_balance_overflow() {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN_ADDR);
        accounting.balances.insert(ALICE, U256::ONE);
        accounting.balances.insert(BOB, U256::MAX);
        let mut token = TestToken::with_storage_and_policy(accounting, InMemoryPolicy::new());

        assert!(token.transfer(ALICE, BOB, U256::ONE, true).is_err());
    }

    // ---- Policy guards (external policy registry path) ----

    #[test]
    fn transfer_allowed_by_external_sender_policy_succeeds() {
        const POLICY_ID: u64 = 7;
        let mut accounting = InMemoryTokenAccounting::new(TOKEN_ADDR);
        accounting.balances.insert(ALICE, U256::from(10u64));
        accounting.policy_ids.insert(B20PolicyType::TransferSender.id(), POLICY_ID);
        let mut policy = InMemoryPolicy::new();
        policy.allow(POLICY_ID, ALICE);
        let mut token = TestToken::with_storage_and_policy(accounting, policy);

        token.transfer(ALICE, BOB, U256::ONE, false).unwrap();

        assert_eq!(token.accounting().balance_of(BOB).unwrap(), U256::ONE);
    }

    #[test]
    fn transfer_reverts_when_denied_by_external_sender_policy() {
        const POLICY_ID: u64 = 7;
        let mut accounting = InMemoryTokenAccounting::new(TOKEN_ADDR);
        accounting.balances.insert(ALICE, U256::from(10u64));
        accounting.policy_ids.insert(B20PolicyType::TransferSender.id(), POLICY_ID);
        // ALICE is not in the allow-list so the external policy denies her.
        let mut token = TestToken::with_storage_and_policy(accounting, InMemoryPolicy::new());

        assert_eq!(
            token.transfer(ALICE, BOB, U256::ONE, false).unwrap_err(),
            BasePrecompileError::revert(IB20::PolicyForbids {
                policyScope: B20PolicyType::TransferSender.id(),
                policyId: POLICY_ID,
            })
        );
    }

    #[test]
    fn transfer_allowed_by_external_receiver_policy_succeeds() {
        const POLICY_ID: u64 = 8;
        let mut accounting = InMemoryTokenAccounting::new(TOKEN_ADDR);
        accounting.balances.insert(ALICE, U256::from(10u64));
        accounting.policy_ids.insert(B20PolicyType::TransferReceiver.id(), POLICY_ID);
        let mut policy = InMemoryPolicy::new();
        policy.allow(POLICY_ID, BOB);
        let mut token = TestToken::with_storage_and_policy(accounting, policy);

        token.transfer(ALICE, BOB, U256::ONE, false).unwrap();

        assert_eq!(token.accounting().balance_of(BOB).unwrap(), U256::ONE);
    }

    // ---- Event content ----

    #[test]
    fn transfer_emits_transfer_event_with_correct_fields() {
        let mut token = token_with_balance(U256::from(100u64));

        token.transfer(ALICE, BOB, U256::from(40u64), false).unwrap();

        assert_eq!(token.accounting().events.len(), 1);
        let decoded = IB20::Transfer::decode_log_data(&token.accounting().events[0]).unwrap();
        assert_eq!(decoded.from, ALICE);
        assert_eq!(decoded.to, BOB);
        assert_eq!(decoded.amount, U256::from(40u64));
    }

    #[test]
    fn transfer_from_emits_transfer_event_with_correct_fields() {
        let mut token = token_with_balance(U256::from(100u64));
        token.accounting_mut().allowances.insert((ALICE, SPENDER), U256::MAX);

        token.transfer_from(SPENDER, ALICE, BOB, U256::from(30u64), false).unwrap();

        assert_eq!(token.accounting().events.len(), 1);
        let decoded = IB20::Transfer::decode_log_data(&token.accounting().events[0]).unwrap();
        assert_eq!(decoded.from, ALICE);
        assert_eq!(decoded.to, BOB);
        assert_eq!(decoded.amount, U256::from(30u64));
    }

    #[test]
    fn transfer_from_with_memo_emits_transfer_then_memo() {
        let mut token = token_with_balance(U256::from(100u64));
        token.accounting_mut().allowances.insert((ALICE, SPENDER), U256::MAX);

        token
            .transfer_from_with_memo(
                SPENDER,
                ALICE,
                BOB,
                U256::from(10u64),
                B256::repeat_byte(0x42),
                false,
            )
            .unwrap();

        assert_eq!(token.accounting().events.len(), 2);
        // First event must be the Transfer.
        IB20::Transfer::decode_log_data(&token.accounting().events[0]).unwrap();
    }
}
