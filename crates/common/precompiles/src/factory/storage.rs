use alloc::string::{String, ToString};

use alloy_primitives::{Address, Bytes, U256, address};
use alloy_sol_types::{SolCall, SolValue};
use base_precompile_macros::contract;
use base_precompile_storage::{BasePrecompileError, Handler, Result};
use revm::state::Bytecode;

use super::variant::TokenVariant;
use crate::{
    B20SecurityStorage, B20SecurityToken, B20Token, B20TokenRole, B20TokenStorage, ITokenFactory,
    PolicyHandle, RoleManaged, Token,
};

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

    /// Creates a token at a deterministic address derived from `(caller, variant, salt)`.
    pub fn create_token(
        &mut self,
        caller: Address,
        call: ITokenFactory::createTokenCall,
    ) -> Result<Address> {
        let Some(variant) = Self::token_variant(call.variant) else {
            return Err(BasePrecompileError::revert(ITokenFactory::InvalidVariant {}));
        };
        let token_params = DecodedCreateParams::decode(variant, &call.params)?;
        Self::check_version(token_params.version)?;
        token_params.validate()?;
        let (token_address, _) = variant.compute_address(caller, call.salt);

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

        match variant {
            TokenVariant::B20 | TokenVariant::Stablecoin => {
                let mut token = B20Token::with_storage_and_policy(
                    B20TokenStorage::from_address(token_address, self.storage),
                    PolicyHandle::new(self.storage),
                );
                token.accounting_mut().name.write(token_params.name.clone())?;
                token.accounting_mut().symbol.write(token_params.symbol.clone())?;
                token.accounting_mut().supply_cap.write(Self::DEFAULT_SUPPLY_CAP)?;
                token.accounting_mut().minimum_redeemable.write(token_params.minimum_redeemable)?;
                token
                    .accounting_mut()
                    .stablecoin_currency
                    .write(token_params.stablecoin_currency)?;
                token.accounting_mut().security_isin.write(token_params.security_isin)?;

                self.emit_event(ITokenFactory::TokenCreated {
                    token: token_address,
                    variant: call.variant,
                    name: token_params.name,
                    symbol: token_params.symbol,
                    decimals: token_params.decimals,
                })?;

                if !token_params.initial_admin.is_zero() {
                    token.grant_role_unchecked(
                        B20TokenRole::DefaultAdmin.id(),
                        token_params.initial_admin,
                        Self::ADDRESS,
                    )?;
                }

                for (index, calldata) in call.initCalls.into_iter().enumerate() {
                    token
                        .inner_with_privilege(self.storage, &calldata, true)
                        .map_err(|err| Self::map_init_call_error(index, err))?;
                }
            }
            TokenVariant::Security => {
                let mut storage = B20SecurityStorage::from_address(token_address, self.storage);
                storage.initialize(
                    token_params.name.clone(),
                    token_params.symbol.clone(),
                    Self::DEFAULT_SUPPLY_CAP,
                    alloy_primitives::U256::from(1_000_000_000_000_000_000u128), // 1:1 ratio
                    token_params.security_isin,
                    token_params.minimum_redeemable,
                )?;

                self.emit_event(ITokenFactory::TokenCreated {
                    token: token_address,
                    variant: call.variant,
                    name: token_params.name,
                    symbol: token_params.symbol,
                    decimals: token_params.decimals,
                })?;

                for (index, calldata) in call.initCalls.into_iter().enumerate() {
                    B20SecurityToken::with_storage_and_policy(
                        B20SecurityStorage::from_address(token_address, self.storage),
                        PolicyHandle::new(self.storage),
                    )
                    .inner(self.storage, &calldata)
                    .map_err(|_| {
                        BasePrecompileError::revert(ITokenFactory::InitCallFailed {
                            index: U256::from(index),
                        })
                    })?;
                }
            }
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
            ITokenFactory::TokenVariant::STABLECOIN => Some(TokenVariant::Stablecoin),
            ITokenFactory::TokenVariant::SECURITY => Some(TokenVariant::Security),
            ITokenFactory::TokenVariant::NONE | ITokenFactory::TokenVariant::__Invalid => None,
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

    fn check_version(version: u8) -> Result<()> {
        if version != Self::CREATE_TOKEN_VERSION {
            return Err(BasePrecompileError::revert(ITokenFactory::UnsupportedVersion { version }));
        }
        Ok(())
    }

    fn map_init_call_error(index: usize, err: BasePrecompileError) -> BasePrecompileError {
        match err {
            BasePrecompileError::Revert(bytes) if !bytes.is_empty() => {
                BasePrecompileError::Revert(bytes)
            }
            err if err.is_system_error() => err,
            _ => BasePrecompileError::revert(ITokenFactory::InitCallFailed {
                index: U256::from(index),
            }),
        }
    }
}

#[derive(Debug)]
struct DecodedCreateParams {
    variant: TokenVariant,
    version: u8,
    name: String,
    symbol: String,
    initial_admin: Address,
    decimals: u8,
    minimum_redeemable: U256,
    stablecoin_currency: String,
    security_isin: String,
}

impl DecodedCreateParams {
    fn decode(variant: TokenVariant, params: &Bytes) -> Result<Self> {
        match variant {
            TokenVariant::B20 => {
                let params = ITokenFactory::B20CreateParams::abi_decode(params)
                    .map_err(Self::invalid_params)?;
                Ok(Self {
                    variant,
                    version: params.version,
                    name: params.name,
                    symbol: params.symbol,
                    initial_admin: params.initialAdmin,
                    decimals: TokenVariant::B20.decimals(),
                    minimum_redeemable: U256::ZERO,
                    stablecoin_currency: String::new(),
                    security_isin: String::new(),
                })
            }
            TokenVariant::Stablecoin => {
                let params = ITokenFactory::B20StablecoinCreateParams::abi_decode(params)
                    .map_err(Self::invalid_params)?;
                Ok(Self {
                    variant,
                    version: params.version,
                    name: params.name,
                    symbol: params.symbol,
                    initial_admin: params.initialAdmin,
                    decimals: TokenVariant::Stablecoin.decimals(),
                    minimum_redeemable: U256::ZERO,
                    stablecoin_currency: params.currency,
                    security_isin: String::new(),
                })
            }
            TokenVariant::Security => {
                let params = ITokenFactory::B20SecurityCreateParams::abi_decode(params)
                    .map_err(Self::invalid_params)?;
                Ok(Self {
                    variant,
                    version: params.version,
                    name: params.name,
                    symbol: params.symbol,
                    initial_admin: params.initialAdmin,
                    decimals: TokenVariant::Security.decimals(),
                    minimum_redeemable: params.minimumRedeemable,
                    stablecoin_currency: String::new(),
                    security_isin: params.isin,
                })
            }
        }
    }

    fn invalid_params(error: impl core::fmt::Display) -> BasePrecompileError {
        BasePrecompileError::AbiDecodeFailed {
            selector: ITokenFactory::createTokenCall::SELECTOR,
            error: error.to_string(),
        }
    }

    fn validate(&self) -> Result<()> {
        match self.variant {
            TokenVariant::Stablecoin if self.stablecoin_currency.is_empty() => {
                Err(BasePrecompileError::revert(ITokenFactory::MissingRequiredField {}))
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, address};
    use alloy_sol_types::{SolCall, SolError, SolValue};
    use base_precompile_storage::{HashMapStorageProvider, StorageCtx};

    use super::*;
    use crate::{
        ActivationFeature, ActivationRegistryStorage, B20SecurityStorage, B20Token,
        B20TokenStorage, IB20, Mintable, Permittable, Token, TokenAccounting, Transferable,
    };

    const ACTIVATION_ADMIN: Address = address!("0xcb00000000000000000000000000000000000000");

    fn activate_precompiles(storage: &mut HashMapStorageProvider) {
        storage.set_caller(ACTIVATION_ADMIN);
        for key in [
            ActivationFeature::TokenFactory.id(),
            ActivationFeature::B20Token.id(),
            ActivationFeature::B20Security.id(),
        ] {
            StorageCtx::enter(storage, |ctx| {
                ActivationRegistryStorage::new(ctx).activate(key, Some(ACTIVATION_ADMIN)).unwrap()
            });
        }
    }

    fn token_params(name: &str, symbol: &str) -> ITokenFactory::B20CreateParams {
        ITokenFactory::B20CreateParams {
            version: TokenFactoryStorage::CREATE_TOKEN_VERSION,
            name: name.to_string(),
            symbol: symbol.to_string(),
            initialAdmin: Address::repeat_byte(0xAB),
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
        create_call(ITokenFactory::TokenVariant::DEFAULT, token_params("Test", "TST"), salt)
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
    fn test_token_variant_compute_address_encodes_variant_and_hash_tail() {
        let creator = Address::repeat_byte(0x11);
        let salt = B256::repeat_byte(0x22);
        let (addr, tail) = TokenVariant::B20.compute_address(creator, salt);

        assert_eq!(addr.as_slice()[11..], tail);
        assert!(TokenVariant::is_b20_address(addr));
        assert_eq!(TokenVariant::from_address(addr), Some(TokenVariant::B20));
        assert_eq!(TokenVariant::decimals_of(addr), Some(18));
    }

    #[test]
    fn test_address_derivation_ignores_decimals_and_uses_variant() {
        let creator = Address::repeat_byte(0x11);
        let salt = B256::repeat_byte(0x33);
        let (default_token, _) = TokenVariant::B20.compute_address(creator, salt);
        let (stablecoin, _) = TokenVariant::Stablecoin.compute_address(creator, salt);

        assert_ne!(default_token, stablecoin);
        assert_eq!(TokenVariant::decimals_of(default_token), Some(18));
        assert_eq!(TokenVariant::decimals_of(stablecoin), Some(6));
    }

    #[test]
    fn test_supported_variants_are_b20_prefixes() {
        let creator = Address::repeat_byte(0x11);
        let salt = B256::repeat_byte(0x44);
        let (stablecoin, _) = TokenVariant::compute_address_for_discriminant(creator, 2, salt);
        let (security, _) = TokenVariant::compute_address_for_discriminant(creator, 3, salt);

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
        let (expected_addr, _) = TokenVariant::B20.compute_address(caller, salt);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = TokenFactoryStorage::new(ctx);
            let token = factory.create_token(caller, b20_call(salt)).unwrap();

            assert_eq!(token, expected_addr);
            assert!(ctx.has_bytecode(expected_addr).unwrap());
        });
    }

    #[test]
    fn test_create_token_stores_metadata_and_uses_variant_decimals() {
        let mut storage = HashMapStorageProvider::new(1);
        let caller = Address::repeat_byte(0x55);
        let salt = B256::repeat_byte(0xBB);
        let call = create_call(
            ITokenFactory::TokenVariant::DEFAULT,
            token_params("My Token", "MYT"),
            salt,
        );

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = TokenFactoryStorage::new(ctx);
            let token_addr = factory.create_token(caller, call).unwrap();
            let token = B20TokenStorage::from_address(token_addr, ctx);

            assert_eq!(token.name.read().unwrap(), "My Token");
            assert_eq!(token.symbol.read().unwrap(), "MYT");
            assert_eq!(token.decimals().unwrap(), 18);
            assert_eq!(token.supply_cap().unwrap(), TokenFactoryStorage::DEFAULT_SUPPLY_CAP);
            assert_eq!(TokenVariant::decimals_of(token_addr), Some(18));
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
            token_params("Supply Token", "SUP"),
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
    fn test_create_token_reverts_for_invalid_version_and_variant() {
        let mut storage = HashMapStorageProvider::new(1);
        let caller = Address::repeat_byte(0x55);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = TokenFactoryStorage::new(ctx);

            let mut bad_params = token_params("Bad Version", "BAD");
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
                params: token_params("Bad Variant", "BAD").abi_encode().into(),
                initCalls: Vec::new(),
            };
            assert!(factory.create_token(caller, bad_variant).is_err());
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
            let output = dispatch_factory_revert(ctx, call);
            assert!(output.starts_with(&ITokenFactory::createTokenCall::SELECTOR));
        });
    }

    #[test]
    fn test_create_token_allows_empty_default_name_and_symbol() {
        let mut storage = HashMapStorageProvider::new(1);
        let caller = Address::repeat_byte(0x55);
        let salt = B256::repeat_byte(0x05);
        let call = create_call(ITokenFactory::TokenVariant::DEFAULT, token_params("", ""), salt);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = TokenFactoryStorage::new(ctx);
            let token_addr = factory.create_token(caller, call).unwrap();
            let token = B20TokenStorage::from_address(token_addr, ctx);

            assert_eq!(token.name.read().unwrap(), "");
            assert_eq!(token.symbol.read().unwrap(), "");
        });
    }

    #[test]
    fn test_create_token_reverts_for_missing_stablecoin_currency() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        let params = ITokenFactory::B20StablecoinCreateParams {
            version: TokenFactoryStorage::CREATE_TOKEN_VERSION,
            name: "Stablecoin Token".to_string(),
            symbol: "USD".to_string(),
            initialAdmin: Address::repeat_byte(0xAB),
            currency: String::new(),
        };
        let call = ITokenFactory::createTokenCall {
            variant: ITokenFactory::TokenVariant::STABLECOIN,
            salt: B256::repeat_byte(0x06),
            params: params.abi_encode().into(),
            initCalls: Vec::new(),
        };

        StorageCtx::enter(&mut storage, |ctx| {
            assert_output(
                dispatch_factory_revert(ctx, call),
                ITokenFactory::MissingRequiredField {}.abi_encode(),
            );
        });
    }

    #[test]
    fn test_create_token_checks_stablecoin_version_before_currency() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        let params = ITokenFactory::B20StablecoinCreateParams {
            version: TokenFactoryStorage::CREATE_TOKEN_VERSION + 1,
            name: "Stablecoin Token".to_string(),
            symbol: "USD".to_string(),
            initialAdmin: Address::repeat_byte(0xAB),
            currency: String::new(),
        };
        let call = ITokenFactory::createTokenCall {
            variant: ITokenFactory::TokenVariant::STABLECOIN,
            salt: B256::repeat_byte(0x07),
            params: params.abi_encode().into(),
            initCalls: Vec::new(),
        };

        StorageCtx::enter(&mut storage, |ctx| {
            assert_output(
                dispatch_factory_revert(ctx, call),
                ITokenFactory::UnsupportedVersion {
                    version: TokenFactoryStorage::CREATE_TOKEN_VERSION + 1,
                }
                .abi_encode(),
            );
        });
    }

    #[test]
    fn test_create_token_supports_stablecoin() {
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
            salt: B256::repeat_byte(0x08),
            params: stablecoin_params.abi_encode().into(),
            initCalls: Vec::new(),
        };

        StorageCtx::enter(&mut storage, |ctx| {
            let stablecoin_addr = ITokenFactory::createTokenCall::abi_decode_returns(
                dispatch_factory_success(ctx, stablecoin_call).as_ref(),
            )
            .unwrap();
            let stablecoin = B20TokenStorage::from_address(stablecoin_addr, ctx);
            assert_eq!(stablecoin.stablecoin_currency.read().unwrap(), "USD");
            assert_eq!(TokenVariant::from_address(stablecoin_addr), Some(TokenVariant::Stablecoin));
            assert_eq!(TokenVariant::decimals_of(stablecoin_addr), Some(6));
        });
    }

    #[test]
    fn test_create_security_token_stores_isin_and_ratio() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        let caller = Address::repeat_byte(0x55);
        let salt = B256::repeat_byte(0x09);
        let (expected_addr, _) = TokenVariant::Security.compute_address(caller, salt);

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
            salt,
            params: security_params.abi_encode().into(),
            initCalls: Vec::new(),
        };

        storage.set_caller(caller);
        StorageCtx::enter(&mut storage, |ctx| {
            assert_output(
                dispatch_factory_success(ctx, security_call),
                ITokenFactory::createTokenCall::abi_encode_returns(&expected_addr),
            );
            assert!(ctx.has_bytecode(expected_addr).unwrap());

            let sec_storage = B20SecurityStorage::from_address(expected_addr, ctx);
            assert_eq!(sec_storage.name.read().unwrap(), "Security Token");
            assert_eq!(sec_storage.symbol.read().unwrap(), "SEC");
            assert_eq!(sec_storage.decimals().unwrap(), 6);
            assert_eq!(
                sec_storage.shares_to_tokens_ratio.read().unwrap(),
                U256::from(1_000_000_000_000_000_000u128)
            );
            assert_eq!(sec_storage.minimum_redeemable.read().unwrap(), U256::ONE);
            // ISIN is stored in the security_identifiers mapping under keccak256("ISIN").
            let isin_key = alloy_primitives::keccak256(b"ISIN");
            assert_eq!(
                sec_storage.security_identifiers.at(&isin_key).read().unwrap(),
                "US0000000000"
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
        let (addr, _) = TokenVariant::B20.compute_address(caller, salt);

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
            TokenVariant::compute_address_for_discriminant(caller, 0xff, salt);

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
            let params = token_params("Lifecycle", "LIFE");
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

            token.mint(alice, alice, U256::from(1_000u64), true).unwrap();
            token.transfer(alice, bob, U256::from(300u64)).unwrap();
            token.mint(alice, alice, U256::from(200u64), true).unwrap();

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
        let (expected_token, _) = TokenVariant::B20.compute_address(creator, salt);
        let mut call = create_call(
            ITokenFactory::TokenVariant::DEFAULT,
            token_params("Dispatch Token", "DSP"),
            salt,
        );
        call.initCalls.push(
            IB20::mintCall { to: Address::repeat_byte(0xCD), amount: U256::from(1_000u64) }
                .abi_encode()
                .into(),
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
                IB20::decimalsCall::abi_encode_returns(&18u8),
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
                U256::ZERO.abi_encode(),
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
            let (token_addr, tail) =
                TokenVariant::B20.compute_address(caller, B256::repeat_byte(0x09));
            assert_eq!(token_addr.as_slice()[11..], tail);
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
        let (token_addr, _) = TokenVariant::B20.compute_address(creator, salt);
        let mut call = create_call(
            ITokenFactory::TokenVariant::DEFAULT,
            token_params("Dispatch Token", "DSP"),
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
}
