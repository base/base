use alloy_primitives::{Address, Bytes, U256, address};
use alloy_sol_types::SolValue;
use base_precompile_macros::contract;
use base_precompile_storage::{BasePrecompileError, Handler, Result};
use revm::state::Bytecode;

use super::variant::TokenVariant;
use crate::{B20Token, B20TokenStorage, ITokenFactory, PolicyHandle, TokenAccounting};

/// Singleton precompile address for the `TokenFactory`.
const FACTORY_ADDRESS: Address = address!("b02f000000000000000000000000000000000000");

/// The B-20 token factory precompile.
#[contract(addr = FACTORY_ADDRESS)]
pub struct TokenFactory {}

impl<'a> TokenFactory<'a> {
    /// Singleton precompile address for the `TokenFactory`.
    pub const ADDRESS: Address = FACTORY_ADDRESS;

    /// Current token creation parameter version.
    pub const CREATE_TOKEN_VERSION: u8 = 1;

    /// Addresses whose lower-8-byte value is reserved for protocol bootstrap tokens.
    pub const RESERVED_SIZE: u64 = 1024;

    /// Creates a token at a deterministic address derived from `(caller, variant, decimals, salt)`.
    pub fn create_token(
        &mut self,
        caller: Address,
        call: ITokenFactory::createTokenCall,
    ) -> Result<Address> {
        let p = call.params;
        if p.version != Self::CREATE_TOKEN_VERSION {
            return Err(BasePrecompileError::revert(ITokenFactory::UnsupportedTokenVersion {
                version: p.version,
            }));
        }
        let Some(variant) = TokenVariant::from_discriminant(p.variant) else {
            return Err(BasePrecompileError::revert(ITokenFactory::UnsupportedTokenVariant {
                variant: p.variant,
            }));
        };
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
            variant.compute_address(caller, token_params.decimals, p.salt);

        if lower_bytes < Self::RESERVED_SIZE {
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
            B20Token::with_storage_and_policy(
                B20TokenStorage::from_address(token_address, self.storage),
                PolicyHandle::new(self.storage),
            )
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
        if !TokenVariant::is_b20_address(token) {
            return Ok(false);
        }
        self.storage.with_account_info(token, |info| Ok(!info.is_empty_code_hash()))
    }

    /// Returns the variant discriminant for `token` decoded from its address prefix.
    pub fn variant_of_token(&self, token: Address) -> Result<u8> {
        let Some(variant) = TokenVariant::from_address(token) else {
            return Ok(TokenVariant::NONE_DISCRIMINANT);
        };
        Ok(variant.discriminant())
    }

    /// Returns the decimals encoded in `token`.
    pub fn decimals_of_token(&self, token: Address) -> Result<u8> {
        Ok(TokenVariant::decimals_of(token).unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use alloy_sol_types::{SolCall, SolError, SolValue};
    use base_precompile_storage::{HashMapStorageProvider, StorageCtx};

    use super::*;
    use crate::{
        B20Token, B20TokenStorage, IB20, Mintable, Permittable, Token, TokenAccounting,
        Transferable,
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
                version: TokenFactory::CREATE_TOKEN_VERSION,
                variant,
                requiredParams: params.abi_encode().into(),
                optionalParams: Bytes::new(),
                postCreateCalls: Vec::new(),
                salt,
            },
        }
    }

    fn b20_call(salt: B256) -> ITokenFactory::createTokenCall {
        create_call(
            TokenVariant::B20.discriminant(),
            token_params("Test", "TST", 18, U256::from(1000), U256::MAX),
            salt,
        )
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
        let mut factory = TokenFactory::new(ctx);
        let output = factory.dispatch(ctx, &call.abi_encode()).unwrap();
        assert!(!output.reverted, "factory call reverted: {:?}", output.bytes);
        output.bytes
    }

    fn dispatch_factory_revert(ctx: StorageCtx<'_>, call: impl SolCall) -> Bytes {
        let mut factory = TokenFactory::new(ctx);
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

        assert!(lower_bytes >= TokenFactory::RESERVED_SIZE);
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
    fn test_unsupported_variants_are_not_b20_prefixes() {
        let creator = Address::repeat_byte(0x11);
        let salt = B256::repeat_byte(0x44);
        let (unsupported_stablecoin, _) =
            TokenVariant::compute_address_for_discriminant(creator, 2, 18, salt);
        let (unsupported_security, _) =
            TokenVariant::compute_address_for_discriminant(creator, 3, 18, salt);

        assert!(!TokenVariant::is_supported_discriminant(2));
        assert!(!TokenVariant::is_supported_discriminant(3));
        assert!(!TokenVariant::is_b20_address(unsupported_stablecoin));
        assert!(!TokenVariant::is_b20_address(unsupported_security));
        assert_eq!(TokenVariant::from_address(unsupported_stablecoin), None);
        assert_eq!(TokenVariant::from_address(unsupported_security), None);
    }

    #[test]
    fn test_create_token_deploys_ef_stub() {
        let mut storage = HashMapStorageProvider::new(1);
        let caller = Address::repeat_byte(0x55);
        let salt = B256::repeat_byte(0xAA);
        let (expected_addr, _) = TokenVariant::B20.compute_address(caller, 18, salt);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = TokenFactory::new(ctx);
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
            TokenVariant::B20.discriminant(),
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
            TokenVariant::B20.discriminant(),
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
            factory.create_token(caller, b20_call(salt)).unwrap();
            let result = factory.create_token(caller, b20_call(salt));
            assert!(result.is_err());
        });
    }

    #[test]
    fn test_create_token_reverts_for_invalid_version_variant_and_optional_params() {
        let mut storage = HashMapStorageProvider::new(1);
        let caller = Address::repeat_byte(0x55);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = TokenFactory::new(ctx);

            let mut bad_version = b20_call(B256::repeat_byte(0x01));
            bad_version.params.version = TokenFactory::CREATE_TOKEN_VERSION + 1;
            assert!(factory.create_token(caller, bad_version).is_err());

            let mut bad_variant = b20_call(B256::repeat_byte(0x02));
            bad_variant.params.variant = 2;
            assert!(factory.create_token(caller, bad_variant).is_err());

            let mut unsupported_optional = b20_call(B256::repeat_byte(0x03));
            unsupported_optional.params.optionalParams = Bytes::from_static(&[0x01]);
            assert!(factory.create_token(caller, unsupported_optional).is_err());
        });
    }

    #[test]
    fn test_post_create_calls_execute_against_token() {
        let mut storage = HashMapStorageProvider::new(1);
        let caller = Address::repeat_byte(0x55);
        let salt = B256::repeat_byte(0xDD);
        let mut call = b20_call(salt);
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
        let (addr, _) = TokenVariant::B20.compute_address(caller, 18, salt);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = TokenFactory::new(ctx);
            assert!(!factory.is_b20(addr).unwrap());

            let token = factory.create_token(caller, b20_call(salt)).unwrap();
            assert!(factory.is_b20(token).unwrap());
            assert_eq!(factory.variant_of_token(token).unwrap(), TokenVariant::B20.discriminant());
        });
    }

    #[test]
    fn test_is_b20_false_for_non_prefix_address() {
        let mut storage = HashMapStorageProvider::new(1);
        let random_addr = Address::repeat_byte(0x42);

        StorageCtx::enter(&mut storage, |ctx| {
            let factory = TokenFactory::new(ctx);
            assert!(!factory.is_b20(random_addr).unwrap());
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
                    create_call(TokenVariant::B20.discriminant(), params, B256::repeat_byte(0x12)),
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

    #[test]
    fn test_token_identity_uses_dynamic_address() {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = TokenFactory::new(ctx);
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
        let mut params =
            token_params("Dispatch Token", "DSP", 6, U256::from(1_000u64), U256::from(10_000u64));
        params.minimumRedeemable = U256::from(25u64);
        params.contractURI = "ipfs://dispatch".to_string();

        let mut storage = HashMapStorageProvider::new(1);
        storage.set_caller(creator);

        StorageCtx::enter(&mut storage, |ctx| {
            assert_output(
                dispatch_factory_success(
                    ctx,
                    ITokenFactory::predictTokenAddressCall {
                        creator,
                        variant: TokenVariant::B20_DISCRIMINANT,
                        decimals: 6,
                        salt,
                    },
                ),
                ITokenFactory::predictTokenAddressCall::abi_encode_returns(&expected_token),
            );

            assert_output(
                dispatch_factory_success(
                    ctx,
                    create_call(TokenVariant::B20_DISCRIMINANT, params, B256::repeat_byte(0x31)),
                ),
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
                    ITokenFactory::variantOfCall { token: expected_token },
                ),
                ITokenFactory::variantOfCall::abi_encode_returns(&TokenVariant::B20_DISCRIMINANT),
            );
            assert_output(
                dispatch_factory_success(
                    ctx,
                    ITokenFactory::decimalsOfCall { token: expected_token },
                ),
                ITokenFactory::decimalsOfCall::abi_encode_returns(&6u8),
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
        StorageCtx::enter(&mut storage, |ctx| {
            let caller = Address::repeat_byte(0xCA);
            let (token_addr, lower_bytes) =
                TokenVariant::B20.compute_address(caller, 18, B256::repeat_byte(0x09));
            assert!(lower_bytes >= TokenFactory::RESERVED_SIZE);
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
        let params = token_params("Dispatch Token", "DSP", 18, U256::from(1_000u64), U256::MAX);

        let mut storage = HashMapStorageProvider::new(1);
        storage.set_caller(creator);
        StorageCtx::enter(&mut storage, |ctx| {
            assert_output(
                dispatch_factory_success(
                    ctx,
                    create_call(TokenVariant::B20_DISCRIMINANT, params, salt),
                ),
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
        let mut params = token_params("Bad Token", "BAD", 18, U256::ZERO, U256::MAX);
        params.admin = Address::ZERO;

        let mut storage = HashMapStorageProvider::new(1);
        storage.set_caller(creator);

        StorageCtx::enter(&mut storage, |ctx| {
            assert_output(
                dispatch_factory_revert(
                    ctx,
                    create_call(TokenVariant::B20_DISCRIMINANT, params, salt),
                ),
                ITokenFactory::ZeroAddress {}.abi_encode(),
            );
        });
    }
}
