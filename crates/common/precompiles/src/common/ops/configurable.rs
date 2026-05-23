use alloc::string::String;

use alloy_primitives::{Address, U256};
use alloy_sol_types::SolEvent;
use base_precompile_storage::{BasePrecompileError, Result};

use super::guards::B20Guards;
use crate::{B20TokenRole, IB20, Token, TokenAccounting};

/// Mutable configuration operations: supply cap, metadata, and contract URI updates.
///
/// All methods have default implementations that go through [`Token::accounting`].
/// Implement with an empty body to opt in.
pub trait Configurable: Token {
    /// Updates the supply cap. Requires `DEFAULT_ADMIN_ROLE`. Emits `SupplyCapUpdated`.
    fn update_supply_cap(
        &mut self,
        caller: Address,
        new_cap: U256,
        privileged: bool,
    ) -> Result<()> {
        if !privileged {
            B20Guards::ensure_token_role::<Self>(self, caller, B20TokenRole::DefaultAdmin)?;
        }
        let supply = self.accounting().total_supply()?;
        if new_cap < supply {
            return Err(BasePrecompileError::revert(IB20::InvalidSupplyCap {
                currentSupply: supply,
                proposedCap: new_cap,
            }));
        }
        let old = self.accounting().supply_cap()?;
        self.accounting_mut().set_supply_cap(new_cap)?;
        self.accounting_mut().emit_event(
            IB20::SupplyCapUpdated { updater: caller, oldSupplyCap: old, newSupplyCap: new_cap }
                .encode_log_data(),
        )
    }

    /// Updates the token name. Emits `NameUpdated`.
    fn update_name(&mut self, caller: Address, name: String, privileged: bool) -> Result<()> {
        if !privileged {
            B20Guards::ensure_token_role::<Self>(self, caller, B20TokenRole::Metadata)?;
        }
        self.accounting_mut().set_name(name.clone())?;
        self.accounting_mut()
            .emit_event(IB20::NameUpdated { updater: caller, newName: name }.encode_log_data())
    }

    /// Updates the token symbol. Emits `SymbolUpdated`.
    fn update_symbol(&mut self, caller: Address, symbol: String, privileged: bool) -> Result<()> {
        if !privileged {
            B20Guards::ensure_token_role::<Self>(self, caller, B20TokenRole::Metadata)?;
        }
        self.accounting_mut().set_symbol(symbol.clone())?;
        self.accounting_mut().emit_event(
            IB20::SymbolUpdated { updater: caller, newSymbol: symbol }.encode_log_data(),
        )
    }

    /// Updates the contract URI. Emits `ContractURIUpdated`.
    fn update_contract_uri(
        &mut self,
        caller: Address,
        uri: String,
        privileged: bool,
    ) -> Result<()> {
        if !privileged {
            B20Guards::ensure_token_role::<Self>(self, caller, B20TokenRole::Metadata)?;
        }
        self.accounting_mut().set_contract_uri(uri)?;
        self.accounting_mut().emit_event(IB20::ContractURIUpdated {}.encode_log_data())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};
    use base_precompile_storage::BasePrecompileError;

    use super::Configurable;
    use crate::{
        B20TokenRole, IB20,
        common::{
            Token, TokenAccounting,
            test_utils::{InMemoryPolicy, InMemoryTokenAccounting, TestToken},
        },
    };

    const CALLER: Address = Address::repeat_byte(0xaa);
    const TOKEN_ADDR: Address = Address::repeat_byte(1);

    fn make_token() -> TestToken {
        TestToken::with_storage_and_policy(
            InMemoryTokenAccounting::new(TOKEN_ADDR),
            InMemoryPolicy::new(),
        )
    }

    fn token_with_default_admin(account: Address) -> TestToken {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN_ADDR);
        accounting.roles.insert((B20TokenRole::DefaultAdmin.id(), account), true);
        TestToken::with_storage_and_policy(accounting, InMemoryPolicy::new())
    }

    #[test]
    fn update_supply_cap_updates_cap_and_emits_event() {
        let mut token = make_token();

        token.update_supply_cap(CALLER, U256::from(500u64), true).unwrap();

        assert_eq!(token.accounting().supply_cap().unwrap(), U256::from(500u64));
        assert_eq!(token.accounting().events.len(), 1);
    }

    #[test]
    fn update_supply_cap_below_current_supply_reverts() {
        let mut token = make_token();
        token.accounting_mut().total_supply = U256::from(100u64);

        assert_eq!(
            token.update_supply_cap(CALLER, U256::from(99u64), true).unwrap_err(),
            BasePrecompileError::revert(IB20::InvalidSupplyCap {
                currentSupply: U256::from(100u64),
                proposedCap: U256::from(99u64),
            })
        );
    }

    #[test]
    fn update_name_round_trips_and_emits_event() {
        let mut token = make_token();

        token.update_name(CALLER, "MyToken".into(), true).unwrap();

        assert_eq!(token.accounting().name().unwrap(), "MyToken");
        assert_eq!(token.accounting().events.len(), 1);
    }

    #[test]
    fn update_symbol_round_trips_and_emits_event() {
        let mut token = make_token();

        token.update_symbol(CALLER, "MTK".into(), true).unwrap();

        assert_eq!(token.accounting().symbol().unwrap(), "MTK");
        assert_eq!(token.accounting().events.len(), 1);
    }

    #[test]
    fn update_contract_uri_round_trips_and_emits_event() {
        let mut token = make_token();

        token.update_contract_uri(CALLER, "ipfs://abc".into(), true).unwrap();

        assert_eq!(token.accounting().contract_uri().unwrap(), "ipfs://abc");
        assert_eq!(token.accounting().events.len(), 1);
    }

    #[test]
    fn non_privileged_config_update_without_admin_role_reverts() {
        let mut token = make_token();

        assert_eq!(
            token.update_name(CALLER, "MyToken".into(), false).unwrap_err(),
            BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                account: CALLER,
                neededRole: B20TokenRole::Metadata.id(),
            })
        );
    }

    #[test]
    fn non_privileged_config_update_with_admin_role_succeeds() {
        let mut token = token_with_default_admin(CALLER);
        token.accounting_mut().roles.insert((B20TokenRole::Metadata.id(), CALLER), true);

        token.update_supply_cap(CALLER, U256::from(500u64), false).unwrap();
        token.update_name(CALLER, "MyToken".into(), false).unwrap();
        token.update_symbol(CALLER, "MTK".into(), false).unwrap();
        token.update_contract_uri(CALLER, "ipfs://abc".into(), false).unwrap();

        assert_eq!(token.accounting().supply_cap().unwrap(), U256::from(500u64));
        assert_eq!(token.accounting().name().unwrap(), "MyToken");
        assert_eq!(token.accounting().symbol().unwrap(), "MTK");
        assert_eq!(token.accounting().contract_uri().unwrap(), "ipfs://abc");
        assert_eq!(token.accounting().events.len(), 4);
    }

    fn token_with_metadata_role(account: Address) -> TestToken {
        let mut accounting = InMemoryTokenAccounting::new(TOKEN_ADDR);
        accounting.roles.insert((B20TokenRole::Metadata.id(), account), true);
        TestToken::with_storage_and_policy(accounting, InMemoryPolicy::new())
    }

    #[test]
    fn update_contract_uri_without_metadata_role_reverts() {
        let mut token = make_token();

        assert_eq!(
            token.update_contract_uri(CALLER, "ipfs://abc".into(), false).unwrap_err(),
            BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                account: CALLER,
                neededRole: B20TokenRole::Metadata.id(),
            })
        );
    }

    #[test]
    fn update_contract_uri_with_only_default_admin_reverts() {
        // DEFAULT_ADMIN_ROLE alone is not sufficient; METADATA_ROLE is required.
        let mut token = token_with_default_admin(CALLER);

        assert_eq!(
            token.update_contract_uri(CALLER, "ipfs://abc".into(), false).unwrap_err(),
            BasePrecompileError::revert(IB20::AccessControlUnauthorizedAccount {
                account: CALLER,
                neededRole: B20TokenRole::Metadata.id(),
            })
        );
    }

    #[test]
    fn update_contract_uri_with_metadata_role_succeeds() {
        let mut token = token_with_metadata_role(CALLER);

        token.update_contract_uri(CALLER, "ipfs://xyz".into(), false).unwrap();

        assert_eq!(token.accounting().contract_uri().unwrap(), "ipfs://xyz");
        assert_eq!(token.accounting().events.len(), 1);
    }
}
