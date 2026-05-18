//! Genesis account builders for B-20 token precompiles.

use std::collections::HashMap;

use alloy_genesis::GenesisAccount;
use alloy_primitives::{B256, Bytes, U256};
use base_precompile_storage::{Handler, HashMapStorageProvider, StorageCtx};

use super::{
    CAPABILITY_CAP_MUTABLE, CAPABILITY_PAUSABLE, DEFAULT_TOKEN_ADDRESS, DefaultTokenStorage,
};

/// Builds genesis [`GenesisAccount`] entries for B-20 token precompiles.
pub struct TokenGenesisBuilder;

impl TokenGenesisBuilder {
    /// Returns the genesis [`GenesisAccount`] for the DefaultToken precompile.
    ///
    /// Includes the `0xef` sentinel bytecode (so tools like `cast` recognise it
    /// as deployed) and pre-initialised storage for name, symbol, decimals, and
    /// capabilities.
    pub fn default_token() -> GenesisAccount {
        GenesisAccount {
            code: Some(Bytes::from_static(&[0xef])),
            storage: Some(Self::init_default_token()),
            ..Default::default()
        }
    }

    fn init_default_token() -> HashMap<B256, B256> {
        let mut provider = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut provider, || {
            let mut s = DefaultTokenStorage::new();
            s.name.write("Ether".to_string())?;
            s.symbol.write("ETH".to_string())?;
            s.decimals.write(18u8)?;
            s.supply_cap.write(U256::MAX)?;
            s.capabilities.write(CAPABILITY_PAUSABLE | CAPABILITY_CAP_MUTABLE)?;
            base_precompile_storage::Result::Ok(())
        })
        .expect("default token genesis initialization failed");

        provider
            .into_storage()
            .filter(|(addr, _, val)| *addr == DEFAULT_TOKEN_ADDRESS && *val != U256::ZERO)
            .map(|(_, slot, val)| (B256::from(slot), B256::from(val)))
            .collect()
    }
}
