//! EVM storage adapter for the stablecoin B-20 variant.

use alloc::string::String;

use alloy_primitives::{U256, uint};
use base_tiger_precompile_storage::{BasePrecompileError, Result, StorageLayout};

use crate::{
    B20CoreStorage, B20TokenKind, B20TokenStorage, B20Variant, IB20Factory, StablecoinAccounting,
};

/// Stablecoin-specific B-20 storage layout, rooted at the `base.b20.stablecoin` ERC-7201
/// namespace.
#[derive(Debug)]
pub struct B20StablecoinExtensionStorage;

impl B20StablecoinExtensionStorage {
    /// ERC-7201 namespace id.
    pub const NAMESPACE: &'static str = "base.b20.stablecoin";
    /// ERC-7201 namespace root.
    pub const ROOT: U256 =
        uint!(0x35827975a06ca0e9367ea3129b19441d45d0ca58e30b7693f09e73d0943d6200_U256);
    /// `string currency`: ISO 4217 currency code.
    pub const CURRENCY: U256 = StorageLayout::field(Self::ROOT, 0);
}

impl B20TokenKind for B20StablecoinExtensionStorage {
    /// Returns the fixed decimals of the token's variant, or 0 for a non-B-20 address.
    fn decimals(token: &B20StablecoinStorage<'_>) -> Result<u8> {
        Ok(B20Variant::from_address(token.address).and_then(|v| v.decimals()).unwrap_or(0))
    }
}

/// EVM-backed storage for a stablecoin B-20 token.
pub type B20StablecoinStorage<'a> = B20TokenStorage<'a, B20StablecoinExtensionStorage>;

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

impl B20StablecoinStorage<'_> {
    /// Writes all creation-time fields atomically.
    ///
    /// Validates that `currency` is non-empty and contains only `A-Z` characters
    /// before writing anything; reverts `MissingRequiredField` for empty and
    /// `InvalidCurrency` for non-A-Z values.
    pub fn initialize(&mut self, init: B20StablecoinInit) -> Result<()> {
        if init.currency.is_empty() {
            return Err(BasePrecompileError::revert(IB20Factory::MissingRequiredField {
                field: String::from("currency"),
            }));
        }
        if !init.currency.bytes().all(|b| b.is_ascii_uppercase()) {
            return Err(BasePrecompileError::revert(IB20Factory::InvalidCurrency {
                code: init.currency,
            }));
        }
        self.storage.write_string(self.address, B20CoreStorage::NAME, &init.name)?;
        self.storage.write_string(self.address, B20CoreStorage::SYMBOL, &init.symbol)?;
        self.storage.write(self.address, B20CoreStorage::SUPPLY_CAP, init.supply_cap)?;
        let currency_slot = B20StablecoinExtensionStorage::CURRENCY;
        self.storage.write_string(self.address, currency_slot, &init.currency)
    }
}

impl StablecoinAccounting for B20StablecoinStorage<'_> {
    fn currency(&self) -> Result<String> {
        self.storage.read_string(self.address, B20StablecoinExtensionStorage::CURRENCY)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256, address};
    use base_tiger_precompile_storage::{StorageCtx, StorageLayout, setup_storage};

    use crate::{
        B20CoreStorage, B20StablecoinExtensionStorage, B20StablecoinStorage, StablecoinAccounting,
        TokenAccounting,
    };

    const TOKEN: Address = address!("000000000000000000000000000000000000b022");

    #[test]
    fn stablecoin_namespace_matches_base_std_root() {
        assert_eq!(
            StorageLayout::erc7201_root(B20StablecoinExtensionStorage::NAMESPACE),
            B20StablecoinExtensionStorage::ROOT
        );
        assert_eq!(B20StablecoinExtensionStorage::CURRENCY, B20StablecoinExtensionStorage::ROOT);
    }

    #[test]
    fn stablecoin_currency_is_rooted_at_extension_namespace() {
        let (mut storage, _) = setup_storage();

        StorageCtx::enter(&mut storage, |ctx| {
            let mut token = B20StablecoinStorage::from_address(TOKEN, ctx);
            token.set_name("Stablecoin".into()).unwrap();
            let currency_slot = B20StablecoinExtensionStorage::CURRENCY;
            ctx.write_string(TOKEN, currency_slot, "USD").unwrap();

            assert_eq!(
                ctx.sload(TOKEN, B20CoreStorage::ROOT).unwrap(),
                short_string_word("Stablecoin")
            );
            assert_eq!(
                ctx.sload(TOKEN, B20StablecoinExtensionStorage::ROOT).unwrap(),
                short_string_word("USD")
            );
            assert_eq!(token.currency().unwrap(), "USD");
        });
    }

    fn short_string_word(value: &str) -> U256 {
        let mut word = [0u8; 32];
        word[..value.len()].copy_from_slice(value.as_bytes());
        word[31] = (value.len() * 2) as u8;
        U256::from_be_bytes(word)
    }
}
