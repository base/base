use alloc::string::String;

use alloy_primitives::{Address, U256};
use alloy_sol_types::SolEvent;
use base_precompile_storage::{BasePrecompileError, Result};

use crate::token::{
    IDefaultToken,
    common::{CAPABILITY_CAP_MUTABLE, Token, TokenAccounting},
};

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
            return Err(BasePrecompileError::revert(IDefaultToken::FeatureDisabled {
                capability: CAPABILITY_CAP_MUTABLE,
            }));
        }
        let supply = self.accounting().total_supply()?;
        if new_cap < supply {
            return Err(BasePrecompileError::revert(IDefaultToken::InvalidSupplyCap {
                currentSupply: supply,
                proposedCap: new_cap,
            }));
        }
        let old = self.accounting().supply_cap()?;
        self.accounting_mut().set_supply_cap(new_cap)?;
        self.accounting_mut().emit_event(
            IDefaultToken::SupplyCapUpdated {
                updater: caller,
                oldSupplyCap: old,
                newSupplyCap: new_cap,
            }
            .encode_log_data(),
        )
    }

    /// Updates the token name. Emits `NameUpdated`.
    fn set_name(&mut self, caller: Address, name: String) -> Result<()> {
        self.accounting_mut().set_name(name.clone())?;
        self.accounting_mut().emit_event(
            IDefaultToken::NameUpdated { updater: caller, newName: name }.encode_log_data(),
        )
    }

    /// Updates the token symbol. Emits `SymbolUpdated`.
    fn set_symbol(&mut self, caller: Address, symbol: String) -> Result<()> {
        self.accounting_mut().set_symbol(symbol.clone())?;
        self.accounting_mut().emit_event(
            IDefaultToken::SymbolUpdated { updater: caller, newSymbol: symbol }.encode_log_data(),
        )
    }

    /// Updates the contract URI. Emits `ContractURIUpdated`.
    fn set_contract_uri(&mut self, _caller: Address, uri: String) -> Result<()> {
        self.accounting_mut().set_contract_uri(uri)?;
        self.accounting_mut().emit_event(IDefaultToken::ContractURIUpdated {}.encode_log_data())
    }
}
