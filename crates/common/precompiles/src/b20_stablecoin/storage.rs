//! EVM storage adapter for the stablecoin B-20 variant.

use alloc::string::String;

use alloy_primitives::{Address, U256};
use base_precompile_macros::{StablecoinAccounting, Storable, TokenAccounting, contract};
use base_precompile_storage::{BasePrecompileError, Handler, Result, StorageCtx};

use crate::{B20CoreStorage, IB20Factory};

/// Stablecoin-specific B-20 storage rooted at the `base.b20.stablecoin` ERC-7201 namespace.
#[derive(Debug, Clone, Storable)]
#[namespace("base.b20.stablecoin")]
pub struct B20StablecoinExtensionStorage {
    /// Stablecoin currency identifier.
    #[accessor]
    #[mutator]
    pub currency: String, // offset 0
}

/// EVM-backed storage for a stablecoin B-20 token.
#[contract]
#[derive(TokenAccounting, StablecoinAccounting)]
pub struct B20StablecoinStorage {
    pub b20: B20CoreStorage,
    pub stablecoin: B20StablecoinExtensionStorage,
}

/// Creation-time parameters for a stablecoin B-20 token.
///
/// Passed to [`B20StablecoinStorage::initialize`] to write all fields atomically.
#[derive(Debug)]
pub struct B20StablecoinInit {
    /// ERC-20 token name.
    pub name: String,
    /// ERC-20 token symbol.
    pub symbol: String,
    /// Maximum total supply.
    pub supply_cap: U256,
    /// ISO 4217 fiat currency code (e.g. `"USD"`).
    pub currency: String,
}

impl<'a> B20StablecoinStorage<'a> {
    /// Creates a `B20StablecoinStorage` instance targeting `addr`.
    pub fn from_address(addr: Address, storage: StorageCtx<'a>) -> Self {
        Self::__new(addr, storage)
    }

    /// Writes all creation-time fields atomically.
    ///
    /// Validates that `currency` contains only `A-Z` characters before writing
    /// anything; reverts `ITokenFactory::InvalidCurrency` otherwise.
    pub fn initialize(&mut self, init: B20StablecoinInit) -> Result<()> {
        if init.currency.is_empty() || !init.currency.bytes().all(|b| b.is_ascii_uppercase()) {
            return Err(BasePrecompileError::revert(IB20Factory::InvalidCurrency {
                code: init.currency,
            }));
        }
        self.b20.name.write(init.name)?;
        self.b20.symbol.write(init.symbol)?;
        self.b20.supply_cap.write(init.supply_cap)?;
        self.stablecoin.currency.write(init.currency)
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::String;

    use alloy_primitives::{Address, U256, address, uint};
    use base_precompile_storage::{Handler, StorableType, StorageCtx, setup_storage};

    use crate::{
        B20CoreStorage, B20StablecoinExtensionStorage, B20StablecoinStorage,
        b20_stablecoin::storage::{__packing_b20_stablecoin_extension_storage, slots},
    };

    const TOKEN: Address = address!("000000000000000000000000000000000000b022");
    const B20_ROOT: U256 =
        uint!(0xc78b71fee795ddd74aff64ea9b2474194c938c3196430e10bb5f01ed48434000_U256);
    const STABLECOIN_ROOT: U256 =
        uint!(0x35827975a06ca0e9367ea3129b19441d45d0ca58e30b7693f09e73d0943d6200_U256);

    #[test]
    fn stablecoin_namespaces_match_base_std_roots() {
        assert_eq!(<B20CoreStorage as StorableType>::STORAGE_NAMESPACE_ROOT, B20_ROOT);
        assert_eq!(
            <B20StablecoinExtensionStorage as StorableType>::STORAGE_NAMESPACE_ID,
            "base.b20.stablecoin"
        );
        assert_eq!(
            <B20StablecoinExtensionStorage as StorableType>::STORAGE_NAMESPACE_ROOT,
            STABLECOIN_ROOT
        );

        assert_eq!(slots::B20, B20_ROOT);
        assert_eq!(slots::STABLECOIN, STABLECOIN_ROOT);
        assert_eq!(__packing_b20_stablecoin_extension_storage::CURRENCY_LOC.offset_slots, 0);
    }

    #[test]
    fn stablecoin_currency_is_rooted_at_extension_namespace() {
        let (mut storage, _) = setup_storage();

        StorageCtx::enter(&mut storage, |ctx| {
            let mut token = B20StablecoinStorage::from_address(TOKEN, ctx);
            token.b20.name.write(String::from("Stablecoin")).unwrap();
            token.stablecoin.currency.write(String::from("USD")).unwrap();

            assert_eq!(ctx.sload(TOKEN, B20_ROOT).unwrap(), short_string_word("Stablecoin"));
            assert_eq!(ctx.sload(TOKEN, STABLECOIN_ROOT).unwrap(), short_string_word("USD"));
        });
    }

    fn short_string_word(value: &str) -> U256 {
        let mut word = [0u8; 32];
        word[..value.len()].copy_from_slice(value.as_bytes());
        word[31] = (value.len() * 2) as u8;
        U256::from_be_bytes(word)
    }
}
