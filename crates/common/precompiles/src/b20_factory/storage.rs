use alloy_primitives::{Address, B256, U256, address, b256};
use base_precompile_macros::contract;
use base_precompile_storage::Result;

use crate::{B20_MAX_SUPPLY_CAP, B20Variant};

/// Maximum total supply for all newly-created B-20 tokens.
const DEFAULT_SUPPLY_CAP: U256 = B20_MAX_SUPPLY_CAP;

/// keccak256(0xef)
const FACTORY_MARKER_CODE_HASH: B256 =
    b256!("309b8896ee4c1ff7ec1966155373dee42663b6b40c3fedc70ba501684848d2a3");

/// The B-20 token factory precompile.
#[contract(addr = Self::ADDRESS)]
pub struct B20FactoryStorage {}

impl<'a> B20FactoryStorage<'a> {
    /// Singleton precompile address for the `B20Factory`.
    pub const ADDRESS: Address = address!("B20F000000000000000000000000000000000000");

    /// Initial supply cap for newly created default B-20 tokens.
    pub const DEFAULT_SUPPLY_CAP: U256 = DEFAULT_SUPPLY_CAP;

    /// Returns whether `token` has the structural B-20 prefix.
    ///
    /// This includes reserved or future variant discriminants in the B-20 address range.
    pub fn is_b20(&self, token: Address) -> Result<bool> {
        Ok(B20Variant::has_b20_prefix(token))
    }

    /// Returns whether `token` is a B-20 address that has been initialized by this factory.
    pub fn is_b20_initialized(&self, token: Address) -> Result<bool> {
        if !B20Variant::has_b20_prefix(token) {
            return Ok(false);
        }
        self.storage.with_account_info(token, |info| Ok(info.code_hash == FACTORY_MARKER_CODE_HASH))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, Bytes, address, keccak256};
    use base_precompile_storage::{HashMapStorageProvider, StorageCtx};
    use revm::state::Bytecode;

    use super::FACTORY_MARKER_CODE_HASH;
    use crate::{B20FactoryStorage, B20Variant};

    #[test]
    fn factory_address_matches_canonical_precompile_address() {
        assert_eq!(
            B20FactoryStorage::ADDRESS,
            address!("B20F000000000000000000000000000000000000")
        );
    }

    #[test]
    fn test_token_variant_compute_address_encodes_variant_and_hash_tail() {
        let creator = Address::repeat_byte(0x11);
        let salt = B256::repeat_byte(0x22);
        let (addr, tail) = B20Variant::Asset.compute_address(creator, salt);

        assert_eq!(addr.as_slice()[11..], tail);
        assert!(B20Variant::is_b20_address(addr));
        assert_eq!(B20Variant::from_address(addr), Some(B20Variant::Asset));
    }

    #[test]
    fn test_address_derivation_uses_variant() {
        let creator = Address::repeat_byte(0x11);
        let salt = B256::repeat_byte(0x33);
        let (asset_token, _) = B20Variant::Asset.compute_address(creator, salt);
        let (stablecoin, _) = B20Variant::Stablecoin.compute_address(creator, salt);

        assert_ne!(asset_token, stablecoin);
        assert_eq!(B20Variant::from_address(asset_token), Some(B20Variant::Asset));
        assert_eq!(B20Variant::from_address(stablecoin), Some(B20Variant::Stablecoin));
    }

    #[test]
    fn test_supported_variants_are_b20_prefixes() {
        let creator = Address::repeat_byte(0x11);
        let salt = B256::repeat_byte(0x44);
        let (asset, _) = B20Variant::compute_address_for_discriminant(creator, 0, salt);
        let (stablecoin, _) = B20Variant::compute_address_for_discriminant(creator, 1, salt);

        assert!(B20Variant::is_supported_discriminant(0));
        assert!(B20Variant::is_supported_discriminant(1));
        assert!(!B20Variant::is_supported_discriminant(2));
        assert!(B20Variant::is_b20_address(asset));
        assert!(B20Variant::is_b20_address(stablecoin));
        assert_eq!(B20Variant::from_address(asset), Some(B20Variant::Asset));
        assert_eq!(B20Variant::from_address(stablecoin), Some(B20Variant::Stablecoin));
    }

    #[test]
    fn test_abi_enum_ordinals_match_solidity() {
        assert_eq!(B20Variant::ASSET_DISCRIMINANT, 0);
        assert_eq!(B20Variant::STABLECOIN_DISCRIMINANT, 1);
        assert_eq!(B20Variant::Asset.discriminant(), 0);
        assert_eq!(B20Variant::Stablecoin.discriminant(), 1);
    }

    #[test]
    fn test_is_b20_accepts_future_structural_prefixes() {
        let mut storage = HashMapStorageProvider::new(1);
        let caller = Address::repeat_byte(0x55);
        let salt = B256::repeat_byte(0x13);
        let (future_variant, _) = B20Variant::compute_address_for_discriminant(caller, 0xff, salt);

        StorageCtx::enter(&mut storage, |ctx| {
            let factory = B20FactoryStorage::new(ctx);
            assert!(factory.is_b20(future_variant).unwrap());
            assert_eq!(B20Variant::from_address(future_variant), None);
        });
    }

    #[test]
    fn test_is_b20_false_for_non_prefix_address() {
        let mut storage = HashMapStorageProvider::new(1);
        let random_addr = Address::repeat_byte(0x42);

        StorageCtx::enter(&mut storage, |ctx| {
            let factory = B20FactoryStorage::new(ctx);
            assert!(!factory.is_b20(random_addr).unwrap());
        });
    }

    #[test]
    fn test_factory_marker_code_hash_constant_matches_keccak256_of_marker_byte() {
        assert_eq!(
            FACTORY_MARKER_CODE_HASH,
            keccak256([0xef_u8]),
            "FACTORY_MARKER_CODE_HASH must equal keccak256([0xef])"
        );
    }

    #[test]
    fn test_is_b20_initialized_rejects_arbitrary_code_at_b20_prefix_address() {
        let caller = Address::repeat_byte(0x55);
        let salt = B256::repeat_byte(0x22);
        let (addr, _) = B20Variant::Asset.compute_address(caller, salt);
        let mut storage = HashMapStorageProvider::new(1);

        StorageCtx::enter(&mut storage, |ctx| {
            ctx.set_code(addr, Bytecode::new_legacy(Bytes::from_static(&[0x60, 0x00]))).unwrap();

            let factory = B20FactoryStorage::new(ctx);
            assert!(factory.is_b20(addr).unwrap(), "address must have B20 prefix");
            assert!(
                !factory.is_b20_initialized(addr).unwrap(),
                "arbitrary code at B20-prefix address must not be reported as factory-initialized"
            );
        });
    }

    #[test]
    fn variant_supported_versions_are_nonzero() {
        // Each variant has its own match arm in supported_version() so adding a new
        // variant without an explicit version is a compile error, preventing silent
        // constant sharing.
        assert!(B20Variant::Stablecoin.supported_version() > 0);
        assert!(B20Variant::Asset.supported_version() > 0);
    }
}
