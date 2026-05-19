use alloy_primitives::{Address, B256, Bytes, U256, address, keccak256};
use alloy_sol_types::SolValue;
use base_precompile_macros::contract;
use base_precompile_storage::{BasePrecompileError, Handler, Result};
use revm::state::Bytecode;

use crate::token::{B20Token, B20TokenStorage, TokenAccounting, abi::ITokenFactory};

/// Singleton precompile address for the `TokenFactory`.
pub const FACTORY_ADDRESS: Address = address!("b02f000000000000000000000000000000000000");

/// First byte of every B-20 address.
pub const B20_PREFIX_BYTE: u8 = 0xb0;
/// Second byte of every B-20 address.
pub const B20_PREFIX_MARKER: u8 = 0x20;
/// Current token creation parameter version.
pub const CREATE_TOKEN_VERSION: u8 = 1;

/// Addresses whose lower-8-byte value (as `u64`) is less than this are reserved for
/// protocol-level bootstrap tokens and cannot be created by public `create*` calls.
pub const RESERVED_SIZE: u64 = 1024;

/// Variant discriminant returned by `variantOf` when address has no B-20 prefix.
pub const VARIANT_NONE: u8 = 0;
/// Variant discriminant for default B-20 tokens.
pub const VARIANT_DEFAULT: u8 = 1;

/// Returns `true` if `addr` has the address prefix of any B-20 token variant.
pub fn has_b20_prefix(addr: &Address) -> bool {
    let b = addr.as_slice();
    b[0] == B20_PREFIX_BYTE
        && b[1] == B20_PREFIX_MARKER
        && b[2] == VARIANT_DEFAULT
        && b[4..12] == [0u8; 8]
}

/// Returns the variant discriminant for `addr` based on its address prefix.
pub fn variant_of(addr: &Address) -> u8 {
    if !has_b20_prefix(addr) {
        return VARIANT_NONE;
    }
    addr.as_slice()[2]
}

/// Returns the decimal count encoded in `addr`.
pub fn decimals_of(addr: &Address) -> u8 {
    if !has_b20_prefix(addr) {
        return 0;
    }
    addr.as_slice()[3]
}

/// Builds the B-20 address prefix for `variant` and `decimals`.
pub const fn address_prefix(variant: u8, decimals: u8) -> [u8; 12] {
    [B20_PREFIX_BYTE, B20_PREFIX_MARKER, variant, decimals, 0, 0, 0, 0, 0, 0, 0, 0]
}

/// Computes the deterministic address for a B-20 token.
pub fn compute_b20_address(
    creator: Address,
    variant: u8,
    decimals: u8,
    salt: B256,
) -> (Address, u64) {
    let hash = keccak256((creator, salt).abi_encode());

    let mut lower_bytes_buf = [0u8; 8];
    lower_bytes_buf.copy_from_slice(&hash[..8]);
    let lower_bytes = u64::from_be_bytes(lower_bytes_buf);

    let mut addr_bytes = [0u8; 20];
    addr_bytes[..12].copy_from_slice(&address_prefix(variant, decimals));
    addr_bytes[12..].copy_from_slice(&hash[..8]);

    (Address::from(addr_bytes), lower_bytes)
}

/// Returns whether `variant` is supported by this factory.
pub const fn is_supported_variant(variant: u8) -> bool {
    variant == VARIANT_DEFAULT
}

/// The B-20 token factory precompile.
#[contract(addr = FACTORY_ADDRESS)]
pub struct TokenFactory {}

impl<'a> TokenFactory<'a> {
    /// Creates a token at a deterministic address derived from `(caller, variant, decimals, salt)`.
    pub fn create_token(
        &mut self,
        caller: Address,
        call: ITokenFactory::createTokenCall,
    ) -> Result<Address> {
        let p = call.params;
        if p.version != CREATE_TOKEN_VERSION {
            return Err(BasePrecompileError::revert(ITokenFactory::UnsupportedTokenVersion {
                version: p.version,
            }));
        }
        if !is_supported_variant(p.variant) {
            return Err(BasePrecompileError::revert(ITokenFactory::UnsupportedTokenVariant {
                variant: p.variant,
            }));
        }
        if !p.optionalParams.is_empty() {
            return Err(BasePrecompileError::revert(ITokenFactory::UnsupportedOptionalParams {}));
        }

        let token_params = ITokenFactory::B20TokenParams::abi_decode(&p.requiredParams)
            .map_err(|_| BasePrecompileError::revert(ITokenFactory::InvalidTokenParams {}))?;

        if token_params.admin.is_zero() {
            return Err(BasePrecompileError::revert(ITokenFactory::ZeroAddress {}));
        }
        if token_params.supplyCap < token_params.initialSupply {
            return Err(BasePrecompileError::revert(ITokenFactory::InvalidSupplyCap {}));
        }

        let (token_address, lower_bytes) =
            compute_b20_address(caller, p.variant, token_params.decimals, p.salt);

        if lower_bytes < RESERVED_SIZE {
            return Err(BasePrecompileError::revert(ITokenFactory::AddressReserved {
                token: token_address,
            }));
        }

        let already_deployed =
            self.storage.with_account_info(token_address, |info| Ok(!info.is_empty_code_hash()))?;
        if already_deployed {
            return Err(BasePrecompileError::revert(ITokenFactory::TokenAlreadyExists {
                token: token_address,
            }));
        }

        let checkpoint = self.storage.checkpoint();
        let stub = Bytecode::new_legacy(Bytes::from_static(&[0xef]));
        self.storage.set_code(token_address, stub)?;

        let mut token = B20TokenStorage::from_address(token_address, self.storage);
        token.name.write(token_params.name.clone())?;
        token.symbol.write(token_params.symbol.clone())?;
        token.supply_cap.write(token_params.supplyCap)?;
        token.capabilities.write(token_params.capabilities)?;
        token.minimum_redeemable.write(token_params.minimumRedeemable)?;
        token.contract_uri.write(token_params.contractURI.clone())?;

        if token_params.initialSupply > U256::ZERO {
            if token_params.initialSupplyRecipient.is_zero() {
                return Err(BasePrecompileError::revert(ITokenFactory::ZeroAddress {}));
            }
            token.total_supply.write(token_params.initialSupply)?;
            token.set_balance(token_params.initialSupplyRecipient, token_params.initialSupply)?;
        }

        for calldata in p.postCreateCalls {
            B20Token::with_storage(B20TokenStorage::from_address(token_address, self.storage))
                .inner(self.storage, &calldata)?;
        }

        self.emit_event(ITokenFactory::TokenCreated {
            token: token_address,
            creator: caller,
            admin: token_params.admin,
            variant: p.variant,
            decimals: token_params.decimals,
            name: token_params.name,
            symbol: token_params.symbol,
            capabilities: token_params.capabilities,
            initialSupply: token_params.initialSupply,
            salt: p.salt,
        })?;

        checkpoint.commit();
        Ok(token_address)
    }

    /// Returns whether `token` is a deployed B-20 token (prefix match + non-empty code).
    pub fn is_b20(&self, token: Address) -> Result<bool> {
        if !has_b20_prefix(&token) {
            return Ok(false);
        }
        self.storage.with_account_info(token, |info| Ok(!info.is_empty_code_hash()))
    }

    /// Returns the variant discriminant for `token` decoded from its address prefix.
    pub fn variant_of_token(&self, token: Address) -> Result<u8> {
        Ok(variant_of(&token))
    }

    /// Returns the decimals encoded in `token`.
    pub fn decimals_of_token(&self, token: Address) -> Result<u8> {
        Ok(decimals_of(&token))
    }
}

#[cfg(test)]
mod tests {
    use alloy_sol_types::SolCall;
    use base_precompile_storage::{HashMapStorageProvider, StorageCtx};

    use super::*;
    use crate::token::{
        B20Token, B20TokenStorage, IB20, Mintable, Token, TokenAccounting, Transferable,
    };

    fn token_params(
        name: &str,
        symbol: &str,
        decimals: u8,
        initial_supply: U256,
        supply_cap: U256,
    ) -> ITokenFactory::B20TokenParams {
        ITokenFactory::B20TokenParams {
            name: name.to_string(),
            symbol: symbol.to_string(),
            decimals,
            admin: Address::repeat_byte(0xAB),
            capabilities: U256::ZERO,
            initialSupply: initial_supply,
            initialSupplyRecipient: Address::repeat_byte(0xCD),
            supplyCap: supply_cap,
            minimumRedeemable: U256::ZERO,
            contractURI: "ipfs://test".to_string(),
        }
    }

    fn create_call(
        variant: u8,
        params: ITokenFactory::B20TokenParams,
        salt: B256,
    ) -> ITokenFactory::createTokenCall {
        ITokenFactory::createTokenCall {
            params: ITokenFactory::CreateTokenParams {
                version: CREATE_TOKEN_VERSION,
                variant,
                requiredParams: params.abi_encode().into(),
                optionalParams: Bytes::new(),
                postCreateCalls: Vec::new(),
                salt,
            },
        }
    }

    fn default_call(salt: B256) -> ITokenFactory::createTokenCall {
        create_call(
            VARIANT_DEFAULT,
            token_params("Test", "TST", 18, U256::from(1000), U256::MAX),
            salt,
        )
    }

    fn token_at<'a>(addr: Address, ctx: StorageCtx<'a>) -> B20Token<B20TokenStorage<'a>> {
        B20Token::with_storage(B20TokenStorage::from_address(addr, ctx))
    }

    #[test]
    fn test_compute_b20_address_encodes_variant_and_decimals() {
        let creator = Address::repeat_byte(0x11);
        let salt = B256::repeat_byte(0x22);
        let (addr, lower_bytes) = compute_b20_address(creator, VARIANT_DEFAULT, 6, salt);

        assert!(lower_bytes >= RESERVED_SIZE);
        assert!(has_b20_prefix(&addr));
        assert_eq!(variant_of(&addr), VARIANT_DEFAULT);
        assert_eq!(decimals_of(&addr), 6);
    }

    #[test]
    fn test_different_decimals_produce_different_addresses() {
        let creator = Address::repeat_byte(0x11);
        let salt = B256::repeat_byte(0x33);
        let (six, _) = compute_b20_address(creator, VARIANT_DEFAULT, 6, salt);
        let (eighteen, _) = compute_b20_address(creator, VARIANT_DEFAULT, 18, salt);

        assert_ne!(six, eighteen);
        assert_eq!(decimals_of(&six), 6);
        assert_eq!(decimals_of(&eighteen), 18);
    }

    #[test]
    fn test_unsupported_variants_are_not_b20_prefixes() {
        let creator = Address::repeat_byte(0x11);
        let salt = B256::repeat_byte(0x44);
        let (unsupported_stablecoin, _) = compute_b20_address(creator, 2, 18, salt);
        let (unsupported_security, _) = compute_b20_address(creator, 3, 18, salt);

        assert!(!is_supported_variant(2));
        assert!(!is_supported_variant(3));
        assert!(!has_b20_prefix(&unsupported_stablecoin));
        assert!(!has_b20_prefix(&unsupported_security));
        assert_eq!(variant_of(&unsupported_stablecoin), VARIANT_NONE);
        assert_eq!(variant_of(&unsupported_security), VARIANT_NONE);
    }

    #[test]
    fn test_create_token_deploys_ef_stub() {
        let mut storage = HashMapStorageProvider::new(1);
        let caller = Address::repeat_byte(0x55);
        let salt = B256::repeat_byte(0xAA);
        let (expected_addr, _) = compute_b20_address(caller, VARIANT_DEFAULT, 18, salt);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = TokenFactory::new(ctx);
            let token = factory.create_token(caller, default_call(salt)).unwrap();

            assert_eq!(token, expected_addr);
            assert!(ctx.has_bytecode(expected_addr).unwrap());
        });
    }

    #[test]
    fn test_create_token_stores_metadata_and_parses_decimals_from_address() {
        let mut storage = HashMapStorageProvider::new(1);
        let caller = Address::repeat_byte(0x55);
        let salt = B256::repeat_byte(0xBB);
        let call = create_call(
            VARIANT_DEFAULT,
            token_params("My Token", "MYT", 6, U256::ZERO, U256::MAX),
            salt,
        );

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = TokenFactory::new(ctx);
            let token_addr = factory.create_token(caller, call).unwrap();
            let token = B20TokenStorage::from_address(token_addr, ctx);

            assert_eq!(token.name.read().unwrap(), "My Token");
            assert_eq!(token.symbol.read().unwrap(), "MYT");
            assert_eq!(token.decimals().unwrap(), 6);
            assert_eq!(factory.decimals_of_token(token_addr).unwrap(), 6);
        });
    }

    #[test]
    fn test_create_token_mints_initial_supply() {
        let mut storage = HashMapStorageProvider::new(1);
        let caller = Address::repeat_byte(0x55);
        let salt = B256::repeat_byte(0xCC);
        let recipient = Address::repeat_byte(0xCD);
        let supply = U256::from(5_000u64);
        let call = create_call(
            VARIANT_DEFAULT,
            token_params("Supply Token", "SUP", 18, supply, U256::MAX),
            salt,
        );

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = TokenFactory::new(ctx);
            let token_addr = factory.create_token(caller, call).unwrap();
            let token = B20TokenStorage::from_address(token_addr, ctx);

            assert_eq!(token.total_supply.read().unwrap(), supply);
            assert_eq!(token.balance_of(recipient).unwrap(), supply);
        });
    }

    #[test]
    fn test_create_token_reverts_if_salt_reused() {
        let mut storage = HashMapStorageProvider::new(1);
        let caller = Address::repeat_byte(0x55);
        let salt = B256::repeat_byte(0xEE);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = TokenFactory::new(ctx);
            factory.create_token(caller, default_call(salt)).unwrap();
            let result = factory.create_token(caller, default_call(salt));
            assert!(result.is_err());
        });
    }

    #[test]
    fn test_create_token_reverts_for_invalid_version_variant_and_optional_params() {
        let mut storage = HashMapStorageProvider::new(1);
        let caller = Address::repeat_byte(0x55);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = TokenFactory::new(ctx);

            let mut bad_version = default_call(B256::repeat_byte(0x01));
            bad_version.params.version = CREATE_TOKEN_VERSION + 1;
            assert!(factory.create_token(caller, bad_version).is_err());

            let mut bad_variant = default_call(B256::repeat_byte(0x02));
            bad_variant.params.variant = 2;
            assert!(factory.create_token(caller, bad_variant).is_err());

            let mut unsupported_optional = default_call(B256::repeat_byte(0x03));
            unsupported_optional.params.optionalParams = Bytes::from_static(&[0x01]);
            assert!(factory.create_token(caller, unsupported_optional).is_err());
        });
    }

    #[test]
    fn test_post_create_calls_execute_against_token() {
        let mut storage = HashMapStorageProvider::new(1);
        let caller = Address::repeat_byte(0x55);
        let salt = B256::repeat_byte(0xDD);
        let mut call = default_call(salt);
        call.params
            .postCreateCalls
            .push(IB20::setNameCall { newName: "Configured".to_string() }.abi_encode().into());

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = TokenFactory::new(ctx);
            let token_addr = factory.create_token(caller, call).unwrap();
            let token = B20TokenStorage::from_address(token_addr, ctx);

            assert_eq!(token.name.read().unwrap(), "Configured");
        });
    }

    #[test]
    fn test_is_b20_and_variant_false_before_create_true_after_create() {
        let mut storage = HashMapStorageProvider::new(1);
        let caller = Address::repeat_byte(0x55);
        let salt = B256::repeat_byte(0x11);
        let (addr, _) = compute_b20_address(caller, VARIANT_DEFAULT, 18, salt);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = TokenFactory::new(ctx);
            assert!(!factory.is_b20(addr).unwrap());

            let token = factory.create_token(caller, default_call(salt)).unwrap();
            assert!(factory.is_b20(token).unwrap());
            assert_eq!(factory.variant_of_token(token).unwrap(), VARIANT_DEFAULT);
        });
    }

    #[test]
    fn test_transfer_and_mint_lifecycle() {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = TokenFactory::new(ctx);
            let mut params = token_params("Lifecycle", "LIFE", 18, U256::from(1_000u64), U256::MAX);
            params.capabilities = U256::from(0b11u64);
            let token_addr = factory
                .create_token(
                    Address::repeat_byte(0xCA),
                    create_call(VARIANT_DEFAULT, params, B256::repeat_byte(0x12)),
                )
                .unwrap();

            let alice = Address::repeat_byte(0xCD);
            let bob = Address::repeat_byte(0xBB);
            let mut token = token_at(token_addr, ctx);

            token.transfer(alice, bob, U256::from(300u64)).unwrap();
            token.mint(alice, U256::from(200u64)).unwrap();

            assert_eq!(token.accounting().balance_of(alice).unwrap(), U256::from(900u64));
            assert_eq!(token.accounting().balance_of(bob).unwrap(), U256::from(300u64));
            assert_eq!(token.accounting().total_supply().unwrap(), U256::from(1_200u64));
        });
    }
}
