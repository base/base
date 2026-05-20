use alloc::string::String;

use alloy_primitives::{Address, Bytes, U256, address};
use alloy_sol_types::SolValue;
use base_precompile_macros::contract;
use base_precompile_storage::{BasePrecompileError, Handler, Result};
use revm::state::Bytecode;

use super::variant::TokenVariant;
use crate::{B20Token, B20TokenStorage, ITokenFactory, PolicyHandle};

/// The B-20 token factory precompile.
#[contract(addr = Self::ADDRESS)]
pub struct TokenFactoryStorage {}

impl<'a> TokenFactoryStorage<'a> {
    /// Singleton precompile address for the `TokenFactory`.
    pub const ADDRESS: Address = address!("b20f00000000000000000000000000000000000f");

    /// Current token creation parameter version.
    pub const CREATE_TOKEN_VERSION: u8 = 1;

    /// Initial supply cap for newly created default B-20 tokens.
    pub const DEFAULT_SUPPLY_CAP: U256 = U256::MAX;

    /// Initial capability bits for newly created default B-20 tokens.
    pub const DEFAULT_CAPABILITIES: U256 = U256::from_limbs([3, 0, 0, 0]);

    /// Creates a token at a deterministic address derived from `(caller, variant, decimals, salt)`.
    pub fn create_token(
        &mut self,
        caller: Address,
        call: ITokenFactory::createTokenCall,
    ) -> Result<Address> {
        let Some(variant) = Self::token_variant(call.variant) else {
            return Err(BasePrecompileError::revert(ITokenFactory::InvalidVariant {}));
        };
        let token_params = Self::decode_create_params(variant, &call.params)?;
        let (token_address, _) = variant.compute_address(caller, token_params.2, call.salt);

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
        token.name.write(token_params.0.clone())?;
        token.symbol.write(token_params.1.clone())?;
        token.supply_cap.write(Self::DEFAULT_SUPPLY_CAP)?;
        token.capabilities.write(Self::DEFAULT_CAPABILITIES)?;

        self.emit_event(ITokenFactory::TokenCreated {
            token: token_address,
            variant: call.variant,
            name: token_params.0,
            symbol: token_params.1,
            decimals: token_params.2,
        })?;

        for (index, calldata) in call.initCalls.into_iter().enumerate() {
            B20Token::with_storage_and_policy(
                B20TokenStorage::from_address(token_address, self.storage),
                PolicyHandle::new(self.storage),
            )
            .inner(self.storage, &calldata)
            .map_err(|_| {
                BasePrecompileError::revert(ITokenFactory::InitCallFailed {
                    index: U256::from(index),
                })
            })?;
        }

        checkpoint.commit();
        Ok(token_address)
    }

    /// Returns whether `token` has the structural B-20 prefix.
    ///
    /// This includes reserved or future variant discriminants in the B-20 address range.
    pub fn is_b20(&self, token: Address) -> Result<bool> {
        Ok(TokenVariant::has_b20_prefix(token))
    }

    pub(super) const fn token_variant(
        variant: ITokenFactory::TokenVariant,
    ) -> Option<TokenVariant> {
        match variant {
            ITokenFactory::TokenVariant::DEFAULT => Some(TokenVariant::B20),
            ITokenFactory::TokenVariant::NONE
            | ITokenFactory::TokenVariant::STABLECOIN
            | ITokenFactory::TokenVariant::SECURITY
            | ITokenFactory::TokenVariant::__Invalid => None,
        }
    }

    pub(super) const fn abi_variant(variant: Option<TokenVariant>) -> ITokenFactory::TokenVariant {
        match variant {
            Some(TokenVariant::B20) => ITokenFactory::TokenVariant::DEFAULT,
            Some(TokenVariant::Stablecoin) => ITokenFactory::TokenVariant::STABLECOIN,
            Some(TokenVariant::Security) => ITokenFactory::TokenVariant::SECURITY,
            None => ITokenFactory::TokenVariant::NONE,
        }
    }

    fn decode_create_params(variant: TokenVariant, params: &Bytes) -> Result<(String, String, u8)> {
        match variant {
            TokenVariant::B20 => {
                let params = ITokenFactory::B20CreateParams::abi_decode(params).map_err(|_| {
                    BasePrecompileError::revert(ITokenFactory::InvalidTokenParams {})
                })?;
                Self::check_version(params.version)?;
                if params.name.is_empty() || params.symbol.is_empty() {
                    return Err(BasePrecompileError::revert(
                        ITokenFactory::MissingRequiredField {},
                    ));
                }
                if params.decimals < 2 || params.decimals > 18 {
                    return Err(BasePrecompileError::revert(ITokenFactory::InvalidDecimals {
                        decimals: params.decimals,
                    }));
                }
                // TODO: validate and wire initialAdmin into token ownership/policy setup.
                Ok((params.name, params.symbol, params.decimals))
            }
            TokenVariant::Stablecoin | TokenVariant::Security => {
                Err(BasePrecompileError::revert(ITokenFactory::InvalidVariant {}))
            }
        }
    }

    fn check_version(version: u8) -> Result<()> {
        if version != Self::CREATE_TOKEN_VERSION {
            return Err(BasePrecompileError::revert(ITokenFactory::UnsupportedVersion { version }));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, address};
    use alloy_sol_types::{SolCall, SolError, SolValue};
    use base_precompile_storage::{HashMapStorageProvider, StorageCtx};

    use super::*;
    use crate::{
        ActivationRegistryStorage, B20Token, B20TokenStorage, IB20, Mintable, Permittable, Token,
        TokenAccounting, Transferable,
    };

    const ACTIVATION_ADMIN: Address = address!("0xcb00000000000000000000000000000000000000");

    fn activate_precompiles(storage: &mut HashMapStorageProvider) {
        storage.set_caller(ACTIVATION_ADMIN);
        StorageCtx::enter(storage, |ctx| {
            ActivationRegistryStorage::new(ctx)
                .activate(ActivationRegistryStorage::TOKEN_FACTORY, Some(ACTIVATION_ADMIN))
                .unwrap()
        });
        StorageCtx::enter(storage, |ctx| {
            ActivationRegistryStorage::new(ctx)
                .activate(ActivationRegistryStorage::B20_TOKEN, Some(ACTIVATION_ADMIN))
                .unwrap()
        });
    }

    fn token_params(name: &str, symbol: &str, decimals: u8) -> ITokenFactory::B20CreateParams {
        ITokenFactory::B20CreateParams {
            version: TokenFactoryStorage::CREATE_TOKEN_VERSION,
            name: name.to_string(),
            symbol: symbol.to_string(),
            initialAdmin: Address::repeat_byte(0xAB),
            decimals,
        }
    }

    fn create_call(
        variant: ITokenFactory::TokenVariant,
        params: ITokenFactory::B20CreateParams,
        salt: B256,
    ) -> ITokenFactory::createTokenCall {
        ITokenFactory::createTokenCall {
            variant,
            salt,
            params: params.abi_encode().into(),
            initCalls: Vec::new(),
        }
    }

    fn b20_call(salt: B256) -> ITokenFactory::createTokenCall {
        create_call(ITokenFactory::TokenVariant::DEFAULT, token_params("Test", "TST", 18), salt)
    }

    fn token_at<'a>(
        addr: Address,
        ctx: StorageCtx<'a>,
    ) -> B20Token<B20TokenStorage<'a>, PolicyHandle<'a>> {
        B20Token::with_storage_and_policy(
            B20TokenStorage::from_address(addr, ctx),
            PolicyHandle::new(ctx),
        )
    }

    fn assert_output(output: Bytes, expected: impl AsRef<[u8]>) {
        assert_eq!(output.as_ref(), expected.as_ref());
    }

    fn dispatch_factory_success(ctx: StorageCtx<'_>, call: impl SolCall) -> Bytes {
        let mut factory = TokenFactoryStorage::new(ctx);
        let output = factory.dispatch(ctx, &call.abi_encode()).unwrap();
        assert!(!output.reverted, "factory call reverted: {:?}", output.bytes);
        output.bytes
    }

    fn dispatch_factory_revert(ctx: StorageCtx<'_>, call: impl SolCall) -> Bytes {
        let mut factory = TokenFactoryStorage::new(ctx);
        let output = factory.dispatch(ctx, &call.abi_encode()).unwrap();
        assert!(output.reverted, "factory call unexpectedly succeeded");
        output.bytes
    }

    fn dispatch_b20_success(ctx: StorageCtx<'_>, token_addr: Address, call: impl SolCall) -> Bytes {
        let mut token = token_at(token_addr, ctx);
        let output = token.dispatch(ctx, &call.abi_encode()).unwrap();
        assert!(!output.reverted, "token call reverted: {:?}", output.bytes);
        output.bytes
    }

    #[test]
    fn test_token_variant_compute_address_encodes_variant_and_decimals() {
        let creator = Address::repeat_byte(0x11);
        let salt = B256::repeat_byte(0x22);
        let (addr, lower_bytes) = TokenVariant::B20.compute_address(creator, 6, salt);

        assert_eq!(addr.as_slice()[12..], lower_bytes.to_be_bytes());
        assert!(TokenVariant::is_b20_address(addr));
        assert_eq!(TokenVariant::from_address(addr), Some(TokenVariant::B20));
        assert_eq!(TokenVariant::decimals_of(addr), Some(6));
    }

    #[test]
    fn test_different_decimals_produce_different_addresses() {
        let creator = Address::repeat_byte(0x11);
        let salt = B256::repeat_byte(0x33);
        let (six, _) = TokenVariant::B20.compute_address(creator, 6, salt);
        let (eighteen, _) = TokenVariant::B20.compute_address(creator, 18, salt);

        assert_ne!(six, eighteen);
        assert_eq!(TokenVariant::decimals_of(six), Some(6));
        assert_eq!(TokenVariant::decimals_of(eighteen), Some(18));
    }

    #[test]
    fn test_supported_variants_are_b20_prefixes() {
        let creator = Address::repeat_byte(0x11);
        let salt = B256::repeat_byte(0x44);
        let (stablecoin, _) = TokenVariant::compute_address_for_discriminant(creator, 2, 18, salt);
        let (security, _) = TokenVariant::compute_address_for_discriminant(creator, 3, 18, salt);

        assert!(TokenVariant::is_supported_discriminant(2));
        assert!(TokenVariant::is_supported_discriminant(3));
        assert!(TokenVariant::is_b20_address(stablecoin));
        assert!(TokenVariant::is_b20_address(security));
        assert_eq!(TokenVariant::from_address(stablecoin), Some(TokenVariant::Stablecoin));
        assert_eq!(TokenVariant::from_address(security), Some(TokenVariant::Security));
    }

    #[test]
    fn test_create_token_deploys_ef_stub() {
        let mut storage = HashMapStorageProvider::new(1);
        let caller = Address::repeat_byte(0x55);
        let salt = B256::repeat_byte(0xAA);
        let (expected_addr, _) = TokenVariant::B20.compute_address(caller, 18, salt);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = TokenFactoryStorage::new(ctx);
            let token = factory.create_token(caller, b20_call(salt)).unwrap();

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
            ITokenFactory::TokenVariant::DEFAULT,
            token_params("My Token", "MYT", 6),
            salt,
        );

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = TokenFactoryStorage::new(ctx);
            let token_addr = factory.create_token(caller, call).unwrap();
            let token = B20TokenStorage::from_address(token_addr, ctx);

            assert_eq!(token.name.read().unwrap(), "My Token");
            assert_eq!(token.symbol.read().unwrap(), "MYT");
            assert_eq!(token.decimals().unwrap(), 6);
            assert_eq!(token.supply_cap().unwrap(), TokenFactoryStorage::DEFAULT_SUPPLY_CAP);
            assert_eq!(token.capabilities().unwrap(), TokenFactoryStorage::DEFAULT_CAPABILITIES);
            assert_eq!(TokenVariant::decimals_of(token_addr), Some(6));
        });
    }

    #[test]
    fn test_create_token_init_calls_can_mint_supply() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        let caller = Address::repeat_byte(0x55);
        let salt = B256::repeat_byte(0xCC);
        let recipient = Address::repeat_byte(0xCD);
        let supply = U256::from(5_000u64);
        let mut call = create_call(
            ITokenFactory::TokenVariant::DEFAULT,
            token_params("Supply Token", "SUP", 18),
            salt,
        );
        call.initCalls.push(IB20::mintCall { to: recipient, amount: supply }.abi_encode().into());

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = TokenFactoryStorage::new(ctx);
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
            let mut factory = TokenFactoryStorage::new(ctx);
            factory.create_token(caller, b20_call(salt)).unwrap();
            let result = factory.create_token(caller, b20_call(salt));
            assert!(result.is_err());
        });
    }

    #[test]
    fn test_create_token_reverts_for_invalid_version_variant_and_decimals() {
        let mut storage = HashMapStorageProvider::new(1);
        let caller = Address::repeat_byte(0x55);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = TokenFactoryStorage::new(ctx);

            let mut bad_params = token_params("Bad Version", "BAD", 18);
            bad_params.version = TokenFactoryStorage::CREATE_TOKEN_VERSION + 1;
            let bad_version = create_call(
                ITokenFactory::TokenVariant::DEFAULT,
                bad_params,
                B256::repeat_byte(0x01),
            );
            assert!(factory.create_token(caller, bad_version).is_err());

            let bad_variant = ITokenFactory::createTokenCall {
                variant: ITokenFactory::TokenVariant::NONE,
                salt: B256::repeat_byte(0x02),
                params: token_params("Bad Variant", "BAD", 18).abi_encode().into(),
                initCalls: Vec::new(),
            };
            assert!(factory.create_token(caller, bad_variant).is_err());

            let invalid_decimals = create_call(
                ITokenFactory::TokenVariant::DEFAULT,
                token_params("Bad Decimals", "BAD", 1),
                B256::repeat_byte(0x03),
            );
            assert!(factory.create_token(caller, invalid_decimals).is_err());
        });
    }

    #[test]
    fn test_create_token_reverts_for_invalid_params_encoding() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        let call = ITokenFactory::createTokenCall {
            variant: ITokenFactory::TokenVariant::DEFAULT,
            salt: B256::repeat_byte(0x04),
            params: Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]),
            initCalls: Vec::new(),
        };

        StorageCtx::enter(&mut storage, |ctx| {
            assert_output(
                dispatch_factory_revert(ctx, call),
                ITokenFactory::InvalidTokenParams {}.abi_encode(),
            );
        });
    }

    #[test]
    fn test_create_token_reverts_for_missing_required_fields() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);

        StorageCtx::enter(&mut storage, |ctx| {
            let missing_name = create_call(
                ITokenFactory::TokenVariant::DEFAULT,
                token_params("", "BAD", 18),
                B256::repeat_byte(0x05),
            );
            let missing_symbol = create_call(
                ITokenFactory::TokenVariant::DEFAULT,
                token_params("Bad Symbol", "", 18),
                B256::repeat_byte(0x06),
            );

            assert_output(
                dispatch_factory_revert(ctx, missing_name),
                ITokenFactory::MissingRequiredField {}.abi_encode(),
            );
            assert_output(
                dispatch_factory_revert(ctx, missing_symbol),
                ITokenFactory::MissingRequiredField {}.abi_encode(),
            );
        });
    }

    #[test]
    fn test_create_token_reverts_for_unimplemented_variants() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);

        let stablecoin_params = ITokenFactory::B20StablecoinCreateParams {
            version: TokenFactoryStorage::CREATE_TOKEN_VERSION,
            name: "Stablecoin Token".to_string(),
            symbol: "USD".to_string(),
            initialAdmin: Address::repeat_byte(0xAB),
            currency: "USD".to_string(),
        };
        let stablecoin_call = ITokenFactory::createTokenCall {
            variant: ITokenFactory::TokenVariant::STABLECOIN,
            salt: B256::repeat_byte(0x06),
            params: stablecoin_params.abi_encode().into(),
            initCalls: Vec::new(),
        };
        let security_params = ITokenFactory::B20SecurityCreateParams {
            version: TokenFactoryStorage::CREATE_TOKEN_VERSION,
            name: "Security Token".to_string(),
            symbol: "SEC".to_string(),
            initialAdmin: Address::repeat_byte(0xAB),
            isin: "US0000000000".to_string(),
            minimumRedeemable: U256::ONE,
        };
        let security_call = ITokenFactory::createTokenCall {
            variant: ITokenFactory::TokenVariant::SECURITY,
            salt: B256::repeat_byte(0x07),
            params: security_params.abi_encode().into(),
            initCalls: Vec::new(),
        };

        StorageCtx::enter(&mut storage, |ctx| {
            assert_output(
                dispatch_factory_revert(ctx, stablecoin_call),
                ITokenFactory::InvalidVariant {}.abi_encode(),
            );
            assert_output(
                dispatch_factory_revert(ctx, security_call),
                ITokenFactory::InvalidVariant {}.abi_encode(),
            );
        });
    }

    #[test]
    fn test_post_create_calls_execute_against_token() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        let caller = Address::repeat_byte(0x55);
        let salt = B256::repeat_byte(0xDD);
        let mut call = b20_call(salt);
        call.initCalls
            .push(IB20::setNameCall { newName: "Configured".to_string() }.abi_encode().into());

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = TokenFactoryStorage::new(ctx);
            let token_addr = factory.create_token(caller, call).unwrap();
            let token = B20TokenStorage::from_address(token_addr, ctx);

            assert_eq!(token.name.read().unwrap(), "Configured");
        });
    }

    #[test]
    fn test_is_b20_and_variant_prefix_before_and_after_create() {
        let mut storage = HashMapStorageProvider::new(1);
        let caller = Address::repeat_byte(0x55);
        let salt = B256::repeat_byte(0x11);
        let (addr, _) = TokenVariant::B20.compute_address(caller, 18, salt);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = TokenFactoryStorage::new(ctx);
            assert!(factory.is_b20(addr).unwrap());

            let token = factory.create_token(caller, b20_call(salt)).unwrap();
            assert!(factory.is_b20(token).unwrap());
            assert_eq!(TokenVariant::from_address(token), Some(TokenVariant::B20));
        });
    }

    #[test]
    fn test_is_b20_accepts_future_structural_prefixes() {
        let mut storage = HashMapStorageProvider::new(1);
        let caller = Address::repeat_byte(0x55);
        let salt = B256::repeat_byte(0x13);
        let (future_variant, _) =
            TokenVariant::compute_address_for_discriminant(caller, 0xff, 18, salt);

        StorageCtx::enter(&mut storage, |ctx| {
            let factory = TokenFactoryStorage::new(ctx);
            assert!(factory.is_b20(future_variant).unwrap());
            assert_eq!(TokenVariant::from_address(future_variant), None);
        });
    }

    #[test]
    fn test_is_b20_false_for_non_prefix_address() {
        let mut storage = HashMapStorageProvider::new(1);
        let random_addr = Address::repeat_byte(0x42);

        StorageCtx::enter(&mut storage, |ctx| {
            let factory = TokenFactoryStorage::new(ctx);
            assert!(!factory.is_b20(random_addr).unwrap());
        });
    }

    #[test]
    fn test_transfer_and_mint_lifecycle() {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = TokenFactoryStorage::new(ctx);
            let params = token_params("Lifecycle", "LIFE", 18);
            let token_addr = factory
                .create_token(
                    Address::repeat_byte(0xCA),
                    create_call(
                        ITokenFactory::TokenVariant::DEFAULT,
                        params,
                        B256::repeat_byte(0x12),
                    ),
                )
                .unwrap();

            let alice = Address::repeat_byte(0xCD);
            let bob = Address::repeat_byte(0xBB);
            let mut token = token_at(token_addr, ctx);

            token.mint(alice, U256::from(1_000u64)).unwrap();
            token.transfer(alice, bob, U256::from(300u64)).unwrap();
            token.mint(alice, U256::from(200u64)).unwrap();

            assert_eq!(token.accounting().balance_of(alice).unwrap(), U256::from(900u64));
            assert_eq!(token.accounting().balance_of(bob).unwrap(), U256::from(300u64));
            assert_eq!(token.accounting().total_supply().unwrap(), U256::from(1_200u64));
        });
    }

    #[test]
    fn test_token_identity_uses_dynamic_address() {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = TokenFactoryStorage::new(ctx);
            let first = factory
                .create_token(Address::repeat_byte(0xCA), b20_call(B256::repeat_byte(0x07)))
                .unwrap();
            let second = factory
                .create_token(Address::repeat_byte(0xCA), b20_call(B256::repeat_byte(0x08)))
                .unwrap();

            assert_ne!(first, second);

            let first_token = token_at(first, ctx);
            let second_token = token_at(second, ctx);

            assert_eq!(first_token.token_address(), first);
            assert_eq!(second_token.token_address(), second);

            let (_, _, _, _, first_domain_address, _, _) =
                first_token.eip712_domain(ctx.chain_id()).unwrap();
            let (_, _, _, _, second_domain_address, _, _) =
                second_token.eip712_domain(ctx.chain_id()).unwrap();

            assert_eq!(first_domain_address, first);
            assert_eq!(second_domain_address, second);
            assert_ne!(
                first_token.domain_separator(ctx.chain_id()).unwrap(),
                second_token.domain_separator(ctx.chain_id()).unwrap()
            );
        });
    }

    #[test]
    fn test_factory_dispatch_create_token_predicts_and_initializes_token() {
        let creator = Address::repeat_byte(0xCA);
        let salt = B256::repeat_byte(0x31);
        let (expected_token, _) = TokenVariant::B20.compute_address(creator, 6, salt);
        let mut call = create_call(
            ITokenFactory::TokenVariant::DEFAULT,
            token_params("Dispatch Token", "DSP", 6),
            salt,
        );
        call.initCalls.push(
            IB20::mintCall { to: Address::repeat_byte(0xCD), amount: U256::from(1_000u64) }
                .abi_encode()
                .into(),
        );
        call.initCalls.push(
            IB20::setMinimumRedeemableCall { newMinimum: U256::from(25u64) }.abi_encode().into(),
        );
        call.initCalls.push(
            IB20::setContractURICall { newURI: "ipfs://dispatch".to_string() }.abi_encode().into(),
        );

        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        storage.set_caller(creator);

        StorageCtx::enter(&mut storage, |ctx| {
            assert_output(
                dispatch_factory_success(
                    ctx,
                    ITokenFactory::getTokenAddressCall {
                        variant: ITokenFactory::TokenVariant::DEFAULT,
                        decimals: 6,
                        sender: creator,
                        salt,
                    },
                ),
                ITokenFactory::getTokenAddressCall::abi_encode_returns(&expected_token),
            );
            assert_output(
                dispatch_factory_revert(
                    ctx,
                    ITokenFactory::getTokenAddressCall {
                        variant: ITokenFactory::TokenVariant::NONE,
                        decimals: 6,
                        sender: creator,
                        salt,
                    },
                ),
                ITokenFactory::InvalidVariant {}.abi_encode(),
            );

            assert_output(
                dispatch_factory_success(ctx, call),
                ITokenFactory::createTokenCall::abi_encode_returns(&expected_token),
            );
            assert!(ctx.has_bytecode(expected_token).unwrap());

            assert_output(
                dispatch_factory_success(ctx, ITokenFactory::isB20Call { token: expected_token }),
                ITokenFactory::isB20Call::abi_encode_returns(&true),
            );
            assert_output(
                dispatch_factory_success(
                    ctx,
                    ITokenFactory::getTokenVariantCall { token: expected_token },
                ),
                ITokenFactory::getTokenVariantCall::abi_encode_returns(
                    &ITokenFactory::TokenVariant::DEFAULT,
                ),
            );

            assert_output(
                dispatch_b20_success(ctx, expected_token, IB20::nameCall {}),
                "Dispatch Token".to_string().abi_encode(),
            );
            assert_output(
                dispatch_b20_success(ctx, expected_token, IB20::symbolCall {}),
                "DSP".to_string().abi_encode(),
            );
            assert_output(
                dispatch_b20_success(ctx, expected_token, IB20::decimalsCall {}),
                IB20::decimalsCall::abi_encode_returns(&6u8),
            );
            assert_output(
                dispatch_b20_success(ctx, expected_token, IB20::totalSupplyCall {}),
                U256::from(1_000u64).abi_encode(),
            );
            assert_output(
                dispatch_b20_success(
                    ctx,
                    expected_token,
                    IB20::balanceOfCall { account: Address::repeat_byte(0xCD) },
                ),
                U256::from(1_000u64).abi_encode(),
            );
            assert_output(
                dispatch_b20_success(ctx, expected_token, IB20::minimumRedeemableCall {}),
                U256::from(25u64).abi_encode(),
            );
            assert_output(
                dispatch_b20_success(ctx, expected_token, IB20::contractURICall {}),
                "ipfs://dispatch".to_string().abi_encode(),
            );
        });
    }

    #[test]
    fn test_uninitialized_prefix_token_reverts() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        StorageCtx::enter(&mut storage, |ctx| {
            let caller = Address::repeat_byte(0xCA);
            let (token_addr, lower_bytes) =
                TokenVariant::B20.compute_address(caller, 18, B256::repeat_byte(0x09));
            assert_eq!(token_addr.as_slice()[12..], lower_bytes.to_be_bytes());
            assert!(!ctx.has_bytecode(token_addr).unwrap());

            let mut token = token_at(token_addr, ctx);
            let result = token.dispatch(ctx, &IB20::nameCall {}.abi_encode()).unwrap();

            assert!(result.reverted);
            assert_eq!(result.bytes.as_ref(), IB20::Uninitialized {}.abi_encode());
        });
    }

    #[test]
    fn test_b20_dispatch_transfer_approve_transfer_from() {
        let creator = Address::repeat_byte(0xCA);
        let alice = Address::repeat_byte(0xCD);
        let bob = Address::repeat_byte(0xBB);
        let spender = Address::repeat_byte(0xEE);
        let charlie = Address::repeat_byte(0xCC);
        let salt = B256::repeat_byte(0x32);
        let (token_addr, _) = TokenVariant::B20.compute_address(creator, 18, salt);
        let mut call = create_call(
            ITokenFactory::TokenVariant::DEFAULT,
            token_params("Dispatch Token", "DSP", 18),
            salt,
        );
        call.initCalls
            .push(IB20::mintCall { to: alice, amount: U256::from(1_000u64) }.abi_encode().into());

        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        storage.set_caller(creator);
        StorageCtx::enter(&mut storage, |ctx| {
            assert_output(
                dispatch_factory_success(ctx, call),
                ITokenFactory::createTokenCall::abi_encode_returns(&token_addr),
            );
        });

        storage.set_caller(alice);
        StorageCtx::enter(&mut storage, |ctx| {
            assert_output(
                dispatch_b20_success(
                    ctx,
                    token_addr,
                    IB20::transferCall { to: bob, amount: U256::from(300u64) },
                ),
                true.abi_encode(),
            );
            assert_output(
                dispatch_b20_success(
                    ctx,
                    token_addr,
                    IB20::approveCall { spender, amount: U256::from(250u64) },
                ),
                true.abi_encode(),
            );
        });

        storage.set_caller(spender);
        StorageCtx::enter(&mut storage, |ctx| {
            assert_output(
                dispatch_b20_success(
                    ctx,
                    token_addr,
                    IB20::transferFromCall { from: alice, to: charlie, amount: U256::from(200u64) },
                ),
                true.abi_encode(),
            );
            assert_output(
                dispatch_b20_success(ctx, token_addr, IB20::balanceOfCall { account: alice }),
                U256::from(500u64).abi_encode(),
            );
            assert_output(
                dispatch_b20_success(ctx, token_addr, IB20::balanceOfCall { account: bob }),
                U256::from(300u64).abi_encode(),
            );
            assert_output(
                dispatch_b20_success(ctx, token_addr, IB20::balanceOfCall { account: charlie }),
                U256::from(200u64).abi_encode(),
            );
            assert_output(
                dispatch_b20_success(
                    ctx,
                    token_addr,
                    IB20::allowanceCall { owner: alice, spender },
                ),
                U256::from(50u64).abi_encode(),
            );
        });
    }

    #[test]
    fn test_factory_dispatch_reverts_with_abi_error() {
        let creator = Address::repeat_byte(0xCA);
        let salt = B256::repeat_byte(0x33);
        let params = token_params("Bad Token", "BAD", 1);

        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        storage.set_caller(creator);

        StorageCtx::enter(&mut storage, |ctx| {
            assert_output(
                dispatch_factory_revert(
                    ctx,
                    create_call(ITokenFactory::TokenVariant::DEFAULT, params, salt),
                ),
                ITokenFactory::InvalidDecimals { decimals: 1 }.abi_encode(),
            );
        });
    }
}
