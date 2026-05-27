use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::SolEvent;
use base_precompile_storage::{BasePrecompileError, Result};

use crate::{B20Guards, B20TokenRole, IB20, Token, TokenAccounting};

/// Token burn operations.
///
/// All methods have default implementations that go through [`Token::accounting`].
/// Implement this trait with an empty body to opt in.
pub trait Burnable: Token {
    /// Destroys `amount` tokens from `from`. Emits `Transfer(from, 0x0, amount)`.
    fn burn(
        &mut self,
        caller: Address,
        from: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()> {
        if !privileged {
            B20Guards::ensure_token_role::<Self>(self, caller, B20TokenRole::Burn)?;
        }
        B20Guards::ensure_not_paused::<Self>(self, IB20::PausableFeature::BURN)?;
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
    fn burn_with_memo(
        &mut self,
        caller: Address,
        from: Address,
        amount: U256,
        memo: B256,
        privileged: bool,
    ) -> Result<()> {
        self.burn(caller, from, amount, privileged)?;
        self.accounting_mut().emit_event(IB20::Memo { caller, memo }.encode_log_data())
    }

    /// Destroys `amount` from a policy-blocked account. Emits `Transfer` and `BurnedBlocked`.
    fn burn_blocked(
        &mut self,
        caller: Address,
        from: Address,
        amount: U256,
        privileged: bool,
    ) -> Result<()> {
        if !privileged {
            B20Guards::ensure_token_role::<Self>(self, caller, B20TokenRole::BurnBlocked)?;
        }
        B20Guards::ensure_blocked::<Self>(self, from)?;
        // Intentional asymmetry: BURN_BLOCKED_ROLE replaces BURN_ROLE, but emergency burn pauses
        // still halt every burn path, including burnBlocked.
        self.burn(caller, from, amount, true)?;
        self.accounting_mut()
            .emit_event(IB20::BurnedBlocked { caller, from, amount }.encode_log_data())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};
    use base_precompile_storage::BasePrecompileError;

    use crate::{
        B20PausableFeature, B20PolicyType, B20TokenRole, Burnable, IB20, InMemoryPolicy,
        InMemoryTokenAccounting, PolicyRegistryStorage, TestToken, Token, TokenAccounting,
    };

    const CALLER: Address = Address::repeat_byte(0xcc);
    const ALICE: Address = Address::repeat_byte(0xaa);
    const TOKEN_ADDR: Address = Address::repeat_byte(1);

    fn token_with_balance(balance: U256) -> TestToken {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN_ADDR);
        accounting.balances.insert(ALICE, balance);
        accounting.total_supply = balance;
        TestToken::with_storage_and_policy(accounting, InMemoryPolicy::new())
    }

    fn token_with_role(role: B20TokenRole, account: Address, balance: U256) -> TestToken {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN_ADDR);
        accounting.balances.insert(ALICE, balance);
        accounting.total_supply = balance;
        accounting.roles.insert((role.id(), account), true);
        TestToken::with_storage_and_policy(accounting, InMemoryPolicy::new())
    }

    #[test]
    fn burn_decreases_balance_and_supply() {
        let mut token = token_with_balance(U256::from(100u64));

        token.burn(CALLER, ALICE, U256::from(40u64), true).unwrap();

        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(60u64));
        assert_eq!(token.accounting().total_supply().unwrap(), U256::from(60u64));
        assert_eq!(token.accounting().events.len(), 1);
    }

    #[test]
    fn burn_insufficient_balance_reverts() {
        let mut token = token_with_balance(U256::from(10u64));

        assert_eq!(
            token.burn(CALLER, ALICE, U256::from(11u64), true).unwrap_err(),
            BasePrecompileError::revert(IB20::InsufficientBalance {
                sender: ALICE,
                balance: U256::from(10u64),
                needed: U256::from(11u64),
            })
        );
    }

    #[test]
    fn non_privileged_burn_without_role_reverts() {
        let mut token = token_with_balance(U256::from(10u64));

        assert_eq!(
            token.burn(CALLER, ALICE, U256::ONE, false).unwrap_err(),
            BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                account: CALLER,
                neededRole: B20TokenRole::Burn.id(),
            })
        );
    }

    #[test]
    fn non_privileged_burn_with_role_succeeds() {
        let mut token = token_with_role(B20TokenRole::Burn, CALLER, U256::from(10u64));

        token.burn(CALLER, ALICE, U256::from(4u64), false).unwrap();

        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(6u64));
    }

    #[test]
    fn burn_reverts_when_burn_feature_paused() {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN_ADDR);
        accounting.balances.insert(ALICE, U256::from(10u64));
        accounting.total_supply = U256::from(10u64);
        accounting.paused = B20PausableFeature::mask(IB20::PausableFeature::BURN);
        let mut token = TestToken::with_storage_and_policy(accounting, InMemoryPolicy::new());

        assert_eq!(
            token.burn(CALLER, ALICE, U256::ONE, true).unwrap_err(),
            BasePrecompileError::revert(IB20::ContractPaused {
                feature: IB20::PausableFeature::BURN,
            })
        );
    }

    #[test]
    fn burn_blocked_reverts_when_account_is_not_blocked() {
        let mut token = token_with_balance(U256::from(10u64));

        assert_eq!(
            token.burn_blocked(CALLER, ALICE, U256::ONE, true).unwrap_err(),
            BasePrecompileError::revert(IB20::AccountNotBlocked { account: ALICE })
        );
    }

    #[test]
    fn burn_blocked_burns_blocked_account_and_emits_events() {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN_ADDR);
        accounting.balances.insert(ALICE, U256::from(100u64));
        accounting.total_supply = U256::from(100u64);
        accounting
            .policy_ids
            .insert(B20PolicyType::TransferSender.id(), PolicyRegistryStorage::ALWAYS_BLOCK_ID);
        let mut token = TestToken::with_storage_and_policy(accounting, InMemoryPolicy::new());

        token.burn_blocked(CALLER, ALICE, U256::from(25u64), true).unwrap();

        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(75u64));
        assert_eq!(token.accounting().total_supply().unwrap(), U256::from(75u64));
        assert_eq!(token.accounting().events.len(), 2);
    }

    #[test]
    fn non_privileged_burn_blocked_without_role_reverts() {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN_ADDR);
        accounting.balances.insert(ALICE, U256::from(10u64));
        accounting.total_supply = U256::from(10u64);
        accounting
            .policy_ids
            .insert(B20PolicyType::TransferSender.id(), PolicyRegistryStorage::ALWAYS_BLOCK_ID);
        let mut token = TestToken::with_storage_and_policy(accounting, InMemoryPolicy::new());

        assert_eq!(
            token.burn_blocked(CALLER, ALICE, U256::ONE, false).unwrap_err(),
            BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                account: CALLER,
                neededRole: B20TokenRole::BurnBlocked.id(),
            })
        );
    }
}
