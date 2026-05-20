use alloc::string::String;

use alloy_primitives::{Address, U256};
use alloy_sol_types::SolEvent;
use base_precompile_storage::{BasePrecompileError, Result};

use crate::{CAPABILITY_CAP_MUTABLE, IB20, Token, TokenAccounting};

/// Mutable configuration operations: supply cap, metadata, and contract URI updates.
///
/// All methods have default implementations that go through [`Token::accounting`].
/// Implement with an empty body to opt in.
pub trait Configurable: Token {
    /// Returns whether the `CAP_MUTABLE` capability bit is set on this token.
    fn is_cap_mutable(&self) -> Result<bool> {
        Ok((self.accounting().capabilities()? & CAPABILITY_CAP_MUTABLE) != U256::ZERO)
    }

    /// Updates the supply cap. Requires `CAP_MUTABLE`. Emits `SupplyCapUpdated`.
    fn set_supply_cap(&mut self, caller: Address, new_cap: U256) -> Result<()> {
        if !self.is_cap_mutable()? {
            return Err(BasePrecompileError::revert(IB20::FeatureDisabled {
                capability: CAPABILITY_CAP_MUTABLE,
            }));
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
    fn set_name(&mut self, caller: Address, name: String) -> Result<()> {
        self.accounting_mut().set_name(name.clone())?;
        self.accounting_mut()
            .emit_event(IB20::NameUpdated { updater: caller, newName: name }.encode_log_data())
    }

    /// Updates the token symbol. Emits `SymbolUpdated`.
    fn set_symbol(&mut self, caller: Address, symbol: String) -> Result<()> {
        self.accounting_mut().set_symbol(symbol.clone())?;
        self.accounting_mut().emit_event(
            IB20::SymbolUpdated { updater: caller, newSymbol: symbol }.encode_log_data(),
        )
    }

    /// Updates the contract URI. Emits `ContractURIUpdated`.
    fn set_contract_uri(&mut self, _caller: Address, uri: String) -> Result<()> {
        self.accounting_mut().set_contract_uri(uri)?;
        self.accounting_mut().emit_event(IB20::ContractURIUpdated {}.encode_log_data())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};

    use super::Configurable;
    use crate::common::{
        CAPABILITY_CAP_MUTABLE, Token, TokenAccounting,
        test_utils::{InMemoryPolicy, InMemoryTokenAccounting, TestToken},
    };

    const CALLER: Address = Address::repeat_byte(0xaa);

    fn make_token(caps: U256) -> TestToken {
        let mut acc = InMemoryTokenAccounting::new(Address::repeat_byte(1));
        acc.capabilities = caps;
        TestToken::with_storage_and_policy(acc, InMemoryPolicy::new())
    }

    #[test]
    fn is_cap_mutable_reflects_capability_bit() {
        assert!(make_token(CAPABILITY_CAP_MUTABLE).is_cap_mutable().unwrap());
        assert!(!make_token(U256::ZERO).is_cap_mutable().unwrap());
    }

    #[test]
    fn set_supply_cap_updates_cap_and_emits_event() {
        let mut token = make_token(CAPABILITY_CAP_MUTABLE);
        token.set_supply_cap(CALLER, U256::from(500u64)).unwrap();

        assert_eq!(token.accounting().supply_cap().unwrap(), U256::from(500u64));
        assert_eq!(token.accounting().events.len(), 1);
    }

    #[test]
    fn set_supply_cap_below_current_supply_reverts() {
        let mut token = make_token(CAPABILITY_CAP_MUTABLE);
        token.accounting_mut().total_supply = U256::from(100u64);
        assert!(token.set_supply_cap(CALLER, U256::from(99u64)).is_err());
    }

    #[test]
    fn set_supply_cap_without_capability_reverts() {
        let mut token = make_token(U256::ZERO);
        assert!(token.set_supply_cap(CALLER, U256::from(1000u64)).is_err());
    }

    #[test]
    fn set_name_round_trips_and_emits_event() {
        let mut token = make_token(U256::ZERO);
        token.set_name(CALLER, "MyToken".into()).unwrap();

        assert_eq!(token.accounting().name().unwrap(), "MyToken");
        assert_eq!(token.accounting().events.len(), 1);
    }

    #[test]
    fn set_symbol_round_trips_and_emits_event() {
        let mut token = make_token(U256::ZERO);
        token.set_symbol(CALLER, "MTK".into()).unwrap();

        assert_eq!(token.accounting().symbol().unwrap(), "MTK");
        assert_eq!(token.accounting().events.len(), 1);
    }

    #[test]
    fn set_contract_uri_round_trips_and_emits_event() {
        let mut token = make_token(U256::ZERO);
        token.set_contract_uri(CALLER, "ipfs://abc".into()).unwrap();

        assert_eq!(token.accounting().contract_uri().unwrap(), "ipfs://abc");
        assert_eq!(token.accounting().events.len(), 1);
    }
}
