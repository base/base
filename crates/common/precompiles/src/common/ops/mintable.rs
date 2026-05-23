use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::SolEvent;
use base_precompile_storage::{BasePrecompileError, Result};

use super::guards::B20Guards;
use crate::{B20PolicyType, B20TokenRole, IB20, Token, TokenAccounting};

/// Token minting operations.
///
/// All methods have default implementations that go through [`Token::accounting`].
/// Implement this trait with an empty body to opt in.
pub trait Mintable: Token {
    /// Creates `amount` tokens at `to`. Enforces supply cap. Emits `Transfer(0x0, to, amount)`.
    fn mint(&mut self, caller: Address, to: Address, amount: U256, privileged: bool) -> Result<()> {
        if to == Address::ZERO {
            return Err(BasePrecompileError::revert(IB20::InvalidReceiver { receiver: to }));
        }
        if !privileged {
            B20Guards::ensure_token_role::<Self>(self, caller, B20TokenRole::Mint)?;
        }
        B20Guards::ensure_policy_type::<Self>(self, B20PolicyType::MintReceiver, to)?;
        B20Guards::ensure_not_paused::<Self>(self, IB20::PausableFeature::MINT)?;
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
    fn mint_with_memo(
        &mut self,
        caller: Address,
        to: Address,
        amount: U256,
        memo: B256,
        privileged: bool,
    ) -> Result<()> {
        self.mint(caller, to, amount, privileged)?;
        self.accounting_mut().emit_event(IB20::Memo { caller, memo }.encode_log_data())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};
    use base_precompile_storage::BasePrecompileError;

    use super::Mintable;
    use crate::{
        B20PausableFeature, B20PolicyType, B20TokenRole, IB20, PolicyRegistryStorage,
        common::{
            Token, TokenAccounting,
            test_utils::{InMemoryPolicy, InMemoryTokenAccounting, TestToken},
        },
    };

    const CALLER: Address = Address::repeat_byte(0xcc);
    const ALICE: Address = Address::repeat_byte(0xaa);
    const TOKEN_ADDR: Address = Address::repeat_byte(1);

    fn make_token() -> TestToken {
        TestToken::with_storage_and_policy(
            InMemoryTokenAccounting::new(TOKEN_ADDR),
            InMemoryPolicy::new(),
        )
    }

    fn token_with_role(role: B20TokenRole, account: Address) -> TestToken {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN_ADDR);
        accounting.roles.insert((role.id(), account), true);
        TestToken::with_storage_and_policy(accounting, InMemoryPolicy::new())
    }

    #[test]
    fn mint_increases_balance_and_total_supply() {
        let mut token = make_token();

        token.mint(CALLER, ALICE, U256::from(100u64), true).unwrap();

        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(100u64));
        assert_eq!(token.accounting().total_supply().unwrap(), U256::from(100u64));
        assert_eq!(token.accounting().events.len(), 1);
    }

    #[test]
    fn mint_to_zero_address_reverts() {
        let mut token = make_token();

        assert_eq!(
            token.mint(CALLER, Address::ZERO, U256::ONE, true).unwrap_err(),
            BasePrecompileError::revert(IB20::InvalidReceiver { receiver: Address::ZERO })
        );
    }

    #[test]
    fn mint_allows_supply_cap_boundary() {
        let mut token = make_token();
        token.accounting_mut().supply_cap = U256::from(100u64);

        token.mint(CALLER, ALICE, U256::from(100u64), true).unwrap();

        assert_eq!(token.accounting().total_supply().unwrap(), U256::from(100u64));
    }

    #[test]
    fn mint_reverts_when_supply_cap_exceeded() {
        let mut token = make_token();
        token.accounting_mut().supply_cap = U256::from(50u64);

        assert_eq!(
            token.mint(CALLER, ALICE, U256::from(51u64), true).unwrap_err(),
            BasePrecompileError::revert(IB20::SupplyCapExceeded {
                cap: U256::from(50u64),
                attempted: U256::from(51u64),
            })
        );
    }

    #[test]
    fn mint_accumulates_across_calls() {
        let mut token = make_token();

        token.mint(CALLER, ALICE, U256::from(40u64), true).unwrap();
        token.mint(CALLER, ALICE, U256::from(60u64), true).unwrap();

        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(100u64));
        assert_eq!(token.accounting().total_supply().unwrap(), U256::from(100u64));
    }

    #[test]
    fn non_privileged_mint_without_role_reverts() {
        let mut token = make_token();

        assert_eq!(
            token.mint(CALLER, ALICE, U256::ONE, false).unwrap_err(),
            BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                account: CALLER,
                neededRole: B20TokenRole::Mint.id(),
            })
        );
    }

    #[test]
    fn non_privileged_mint_with_role_succeeds() {
        let mut token = token_with_role(B20TokenRole::Mint, CALLER);

        token.mint(CALLER, ALICE, U256::from(10u64), false).unwrap();

        assert_eq!(token.accounting().balance_of(ALICE).unwrap(), U256::from(10u64));
    }

    #[test]
    fn mint_reverts_when_mint_feature_paused() {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN_ADDR);
        accounting.paused = B20PausableFeature::mask(IB20::PausableFeature::MINT);
        let mut token = TestToken::with_storage_and_policy(accounting, InMemoryPolicy::new());

        assert_eq!(
            token.mint(CALLER, ALICE, U256::ONE, true).unwrap_err(),
            BasePrecompileError::revert(IB20::ContractPaused {
                feature: IB20::PausableFeature::MINT,
            })
        );
    }

    #[test]
    fn mint_reverts_when_receiver_policy_denies() {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN_ADDR);
        accounting
            .policy_ids
            .insert(B20PolicyType::MintReceiver.id(), PolicyRegistryStorage::ALWAYS_BLOCK_ID);
        let mut token = TestToken::with_storage_and_policy(accounting, InMemoryPolicy::new());

        assert_eq!(
            token.mint(CALLER, ALICE, U256::ONE, true).unwrap_err(),
            BasePrecompileError::revert(IB20::PolicyForbids {
                policyScope: B20PolicyType::MintReceiver.id(),
                policyId: PolicyRegistryStorage::ALWAYS_BLOCK_ID,
            })
        );
    }
}
