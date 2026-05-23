use alloc::{string::ToString, vec::Vec};

use alloy_primitives::{Address, Bytes, U256, address};
use alloy_sol_types::{SolCall, SolValue};
use base_precompile_macros::contract;
use base_precompile_storage::{BasePrecompileError, Result};
use revm::state::Bytecode;

use super::variant::B20Variant;
use crate::{
    B20SecurityInit, B20SecurityStorage, B20SecurityToken, B20StablecoinInit, B20StablecoinStorage,
    B20StablecoinToken, B20Token, B20TokenInit, B20TokenRole, B20TokenStorage, IB20Factory,
    PolicyHandle, RoleManaged, Token,
};

/// Maximum total supply for all newly-created B-20 tokens.
const DEFAULT_SUPPLY_CAP: U256 = U256::MAX;

/// Initial share-to-token ratio storage value. Reads treat zero as WAD precision (1:1).
const INITIAL_SHARES_TO_TOKENS_RATIO: U256 = U256::ZERO;

/// The B-20 token factory precompile.
#[contract(addr = Self::ADDRESS)]
pub struct B20FactoryStorage {}

impl<'a> B20FactoryStorage<'a> {
    /// Singleton precompile address for the `B20Factory`.
    pub const ADDRESS: Address = address!("B20F000000000000000000000000000000000000");

    /// Current token creation parameter version.
    pub const CREATE_TOKEN_VERSION: u8 = 1;

    /// Initial supply cap for newly created default B-20 tokens.
    pub const DEFAULT_SUPPLY_CAP: U256 = DEFAULT_SUPPLY_CAP;

    /// Creates a token at a deterministic address derived from `(caller, variant, salt)`.
    pub fn create_b20(
        &mut self,
        caller: Address,
        call: IB20Factory::createB20Call,
    ) -> Result<Address> {
        let variant = B20Variant::from_abi(call.variant)
            .ok_or_else(|| BasePrecompileError::revert(IB20Factory::InvalidVariant {}))?;
        let params = TokenCreateParams::decode(variant, &call.params)?;
        Self::check_version(params.version(), variant.abi())?;
        params.validate()?;
        let (token_address, _) = variant.compute_address(caller, call.salt);

        let already_deployed =
            self.storage.with_account_info(token_address, |info| Ok(!info.is_empty_code_hash()))?;
        if already_deployed {
            return Err(BasePrecompileError::revert(IB20Factory::TokenAlreadyExists {
                token: token_address,
            }));
        }

        let checkpoint = self.storage.checkpoint();
        let stub = Bytecode::new_legacy(Bytes::from_static(&[0xef]));
        self.storage.set_code(token_address, stub)?;

        let init_calls = call.initCalls;
        match params {
            TokenCreateParams::B20 { common, init } => {
                self.init_b20_token(token_address, common, init, init_calls)?;
            }
            TokenCreateParams::Stablecoin { common, init } => {
                self.init_stablecoin(token_address, common, init, init_calls)?;
            }
            TokenCreateParams::Security { common, init } => {
                self.init_security_token(token_address, common, init, init_calls)?;
            }
        }

        checkpoint.commit();
        Ok(token_address)
    }

    /// Returns whether `token` has the structural B-20 prefix.
    ///
    /// This includes reserved or future variant discriminants in the B-20 address range.
    pub fn is_b20(&self, token: Address) -> Result<bool> {
        Ok(B20Variant::has_b20_prefix(token))
    }

    /// Returns whether `token` is a B-20 address that has been initialized by this factory.
    ///
    /// Returns `false` for addresses without the B-20 prefix, even if they have bytecode.
    pub fn is_b20_initialized(&self, token: Address) -> Result<bool> {
        if !B20Variant::has_b20_prefix(token) {
            return Ok(false);
        }
        self.storage.with_account_info(token, |info| Ok(!info.is_empty_code_hash()))
    }

    fn init_b20_token(
        &mut self,
        token_address: Address,
        common: CommonParams,
        init: B20TokenInit,
        init_calls: Vec<Bytes>,
    ) -> Result<()> {
        let mut token = B20Token::with_storage_and_policy(
            B20TokenStorage::from_address(token_address, self.storage),
            PolicyHandle::new(self.storage),
        );
        let (name, symbol) = (init.name.clone(), init.symbol.clone());
        token.accounting_mut().initialize(init)?;

        self.emit_event(IB20Factory::B20Created {
            token: token_address,
            variant: B20Variant::B20.abi(),
            name,
            symbol,
            decimals: B20Variant::B20.decimals(),
        })?;

        if !common.initial_admin.is_zero() {
            token.grant_role_unchecked(
                B20TokenRole::DefaultAdmin.id(),
                common.initial_admin,
                Self::ADDRESS,
            )?;
        }

        self.storage.with_caller(Self::ADDRESS, || {
            for (index, calldata) in init_calls.into_iter().enumerate() {
                token
                    .inner_with_privilege(self.storage, &calldata, true)
                    .map_err(|err| Self::map_init_call_error(index, err))?;
            }
            Ok::<(), BasePrecompileError>(())
        })?;
        Ok(())
    }

    fn init_stablecoin(
        &mut self,
        token_address: Address,
        common: CommonParams,
        init: B20StablecoinInit,
        init_calls: Vec<Bytes>,
    ) -> Result<()> {
        let mut token = B20StablecoinToken::with_storage_and_policy(
            B20StablecoinStorage::from_address(token_address, self.storage),
            PolicyHandle::new(self.storage),
        );
        let (name, symbol) = (init.name.clone(), init.symbol.clone());
        token.accounting_mut().initialize(init)?;

        self.emit_event(IB20Factory::B20Created {
            token: token_address,
            variant: B20Variant::Stablecoin.abi(),
            name,
            symbol,
            decimals: B20Variant::Stablecoin.decimals(),
        })?;

        if !common.initial_admin.is_zero() {
            token.grant_role_unchecked(
                B20TokenRole::DefaultAdmin.id(),
                common.initial_admin,
                Self::ADDRESS,
            )?;
        }

        self.storage.with_caller(Self::ADDRESS, || {
            for (index, calldata) in init_calls.into_iter().enumerate() {
                token
                    .inner_with_privilege(self.storage, &calldata, true)
                    .map_err(|err| Self::map_init_call_error(index, err))?;
            }
            Ok::<(), BasePrecompileError>(())
        })?;
        Ok(())
    }

    fn init_security_token(
        &mut self,
        token_address: Address,
        common: CommonParams,
        init: B20SecurityInit,
        init_calls: Vec<Bytes>,
    ) -> Result<()> {
        let mut storage = B20SecurityStorage::from_address(token_address, self.storage);
        let (name, symbol) = (init.name.clone(), init.symbol.clone());
        storage.initialize(init)?;

        self.emit_event(IB20Factory::B20Created {
            token: token_address,
            variant: B20Variant::Security.abi(),
            name,
            symbol,
            decimals: B20Variant::Security.decimals(),
        })?;

        if !common.initial_admin.is_zero() {
            let mut token = B20SecurityToken::with_storage_and_policy(
                B20SecurityStorage::from_address(token_address, self.storage),
                PolicyHandle::new(self.storage),
            );
            token.grant_role_unchecked(
                B20TokenRole::DefaultAdmin.id(),
                common.initial_admin,
                Self::ADDRESS,
            )?;
        }

        self.storage.with_caller(Self::ADDRESS, || {
            for (index, calldata) in init_calls.into_iter().enumerate() {
                B20SecurityToken::with_storage_and_policy(
                    B20SecurityStorage::from_address(token_address, self.storage),
                    PolicyHandle::new(self.storage),
                )
                .inner_with_privilege(self.storage, &calldata, true)
                .map_err(|err| Self::map_init_call_error(index, err))?;
            }
            Ok::<(), BasePrecompileError>(())
        })?;
        Ok(())
    }

    fn check_version(version: u8, variant: IB20Factory::B20Variant) -> Result<()> {
        if version != Self::CREATE_TOKEN_VERSION {
            return Err(BasePrecompileError::revert(IB20Factory::UnsupportedVersion {
                version,
                variant,
            }));
        }
        Ok(())
    }

    fn map_init_call_error(index: usize, err: BasePrecompileError) -> BasePrecompileError {
        match err {
            BasePrecompileError::Revert(bytes) if !bytes.is_empty() => {
                BasePrecompileError::Revert(bytes)
            }
            err if err.is_system_error() => err,
            _ => BasePrecompileError::revert(IB20Factory::InitCallFailed {
                index: U256::from(index),
            }),
        }
    }
}

/// Control-flow fields shared by every token variant (not written to storage).
#[derive(Debug)]
struct CommonParams {
    version: u8,
    initial_admin: Address,
}

/// Decoded creation parameters typed per token variant.
///
/// Each arm carries a typed `init` struct that maps 1-to-1 to its storage
/// `initialize()` call, plus the shared control-flow fields in `common`.
#[derive(Debug)]
enum TokenCreateParams {
    B20 { common: CommonParams, init: B20TokenInit },
    Stablecoin { common: CommonParams, init: B20StablecoinInit },
    Security { common: CommonParams, init: B20SecurityInit },
}

impl TokenCreateParams {
    fn decode(variant: B20Variant, params: &Bytes) -> Result<Self> {
        match variant {
            B20Variant::B20 => {
                let p = IB20Factory::B20CreateParams::abi_decode(params)
                    .map_err(Self::invalid_params)?;
                Ok(Self::B20 {
                    common: CommonParams { version: p.version, initial_admin: p.initialAdmin },
                    init: B20TokenInit {
                        name: p.name,
                        symbol: p.symbol,
                        supply_cap: DEFAULT_SUPPLY_CAP,
                    },
                })
            }
            B20Variant::Stablecoin => {
                let p = IB20Factory::B20StablecoinCreateParams::abi_decode(params)
                    .map_err(Self::invalid_params)?;
                Ok(Self::Stablecoin {
                    common: CommonParams { version: p.version, initial_admin: p.initialAdmin },
                    init: B20StablecoinInit {
                        name: p.name,
                        symbol: p.symbol,
                        supply_cap: DEFAULT_SUPPLY_CAP,
                        currency: p.currency,
                    },
                })
            }
            B20Variant::Security => {
                let p = IB20Factory::B20SecurityCreateParams::abi_decode(params)
                    .map_err(Self::invalid_params)?;
                Ok(Self::Security {
                    common: CommonParams { version: p.version, initial_admin: p.initialAdmin },
                    init: B20SecurityInit {
                        name: p.name,
                        symbol: p.symbol,
                        supply_cap: DEFAULT_SUPPLY_CAP,
                        shares_to_tokens_ratio: INITIAL_SHARES_TO_TOKENS_RATIO,
                        isin: p.isin,
                        minimum_redeemable: p.minimumRedeemable,
                    },
                })
            }
        }
    }

    const fn version(&self) -> u8 {
        match self {
            Self::B20 { common, .. }
            | Self::Stablecoin { common, .. }
            | Self::Security { common, .. } => common.version,
        }
    }

    /// Validates variant-specific invariants after the shared version check.
    ///
    /// Each arm owns its own rules. Version is checked first by the caller (`check_version`)
    /// so that version errors always take precedence over field-level errors.
    fn validate(&self) -> Result<()> {
        match self {
            Self::B20 { init, .. } => Self::validate_b20(init),
            Self::Stablecoin { init, .. } => Self::validate_stablecoin(init),
            Self::Security { init, .. } => Self::validate_security(init),
        }
    }

    const fn validate_b20(_init: &B20TokenInit) -> Result<()> {
        Ok(())
    }

    const fn validate_stablecoin(_init: &B20StablecoinInit) -> Result<()> {
        // Currency validation is delegated to `B20StablecoinStorage::initialize`, which rejects
        // all invalid values (including empty) with `InvalidCurrency`.
        Ok(())
    }

    fn validate_security(init: &B20SecurityInit) -> Result<()> {
        if init.isin.is_empty() {
            return Err(BasePrecompileError::revert(IB20Factory::MissingRequiredField {}));
        }
        Ok(())
    }

    fn invalid_params(error: impl core::fmt::Display) -> BasePrecompileError {
        BasePrecompileError::AbiDecodeFailed {
            selector: IB20Factory::createB20Call::SELECTOR,
            error: error.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, address};
    use alloy_sol_types::{SolCall, SolError, SolValue};
    use base_precompile_storage::{Handler, HashMapStorageProvider, StorageCtx};

    use super::*;
    use crate::{
        ActivationFeature, ActivationRegistryStorage, B20SecurityStorage, B20Token,
        B20TokenStorage, IB20, Mintable, Permittable, Token, TokenAccounting, Transferable,
    };

    const ACTIVATION_ADMIN: Address = address!("0xcb00000000000000000000000000000000000000");

    #[test]
    fn factory_address_matches_canonical_precompile_address() {
        assert_eq!(
            B20FactoryStorage::ADDRESS,
            address!("B20F000000000000000000000000000000000000")
        );
    }

    fn activate_precompiles(storage: &mut HashMapStorageProvider) {
        storage.set_caller(ACTIVATION_ADMIN);
        for key in [
            ActivationFeature::B20Factory.id(),
            ActivationFeature::B20Token.id(),
            ActivationFeature::B20Stablecoin.id(),
            ActivationFeature::B20Security.id(),
        ] {
            StorageCtx::enter(storage, |ctx| {
                ActivationRegistryStorage::new(ctx).activate(key, Some(ACTIVATION_ADMIN)).unwrap()
            });
        }
    }

    fn token_params(name: &str, symbol: &str) -> IB20Factory::B20CreateParams {
        IB20Factory::B20CreateParams {
            version: B20FactoryStorage::CREATE_TOKEN_VERSION,
            name: name.to_string(),
            symbol: symbol.to_string(),
            initialAdmin: Address::repeat_byte(0xAB),
        }
    }

    fn create_call(
        variant: IB20Factory::B20Variant,
        params: IB20Factory::B20CreateParams,
        salt: B256,
    ) -> IB20Factory::createB20Call {
        IB20Factory::createB20Call {
            variant,
            salt,
            params: params.abi_encode().into(),
            initCalls: Vec::new(),
        }
    }

    fn b20_call(salt: B256) -> IB20Factory::createB20Call {
        create_call(IB20Factory::B20Variant::DEFAULT, token_params("Test", "TST"), salt)
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
        let mut factory = B20FactoryStorage::new(ctx);
        let output = factory.dispatch(ctx, &call.abi_encode()).unwrap();
        assert!(!output.is_revert(), "factory call reverted: {:?}", output.bytes);
        output.bytes
    }

    fn dispatch_factory_revert(ctx: StorageCtx<'_>, call: impl SolCall) -> Bytes {
        let mut factory = B20FactoryStorage::new(ctx);
        let output = factory.dispatch(ctx, &call.abi_encode()).unwrap();
        assert!(output.is_revert(), "factory call unexpectedly succeeded");
        output.bytes
    }

    fn dispatch_b20_success(ctx: StorageCtx<'_>, token_addr: Address, call: impl SolCall) -> Bytes {
        let mut token = token_at(token_addr, ctx);
        let output = token.dispatch(ctx, &call.abi_encode()).unwrap();
        assert!(!output.is_revert(), "token call reverted: {:?}", output.bytes);
        output.bytes
    }

    #[test]
    fn test_token_variant_compute_address_encodes_variant_and_hash_tail() {
        let creator = Address::repeat_byte(0x11);
        let salt = B256::repeat_byte(0x22);
        let (addr, tail) = B20Variant::B20.compute_address(creator, salt);

        assert_eq!(addr.as_slice()[11..], tail);
        assert!(B20Variant::is_b20_address(addr));
        assert_eq!(B20Variant::from_address(addr), Some(B20Variant::B20));
        assert_eq!(B20Variant::decimals_of(addr), Some(18));
    }

    #[test]
    fn test_address_derivation_ignores_decimals_and_uses_variant() {
        let creator = Address::repeat_byte(0x11);
        let salt = B256::repeat_byte(0x33);
        let (default_token, _) = B20Variant::B20.compute_address(creator, salt);
        let (stablecoin, _) = B20Variant::Stablecoin.compute_address(creator, salt);

        assert_ne!(default_token, stablecoin);
        assert_eq!(B20Variant::decimals_of(default_token), Some(18));
        assert_eq!(B20Variant::decimals_of(stablecoin), Some(6));
    }

    #[test]
    fn test_supported_variants_are_b20_prefixes() {
        let creator = Address::repeat_byte(0x11);
        let salt = B256::repeat_byte(0x44);
        let (stablecoin, _) = B20Variant::compute_address_for_discriminant(creator, 1, salt);
        let (security, _) = B20Variant::compute_address_for_discriminant(creator, 2, salt);

        assert!(B20Variant::is_supported_discriminant(1));
        assert!(B20Variant::is_supported_discriminant(2));
        assert!(B20Variant::is_b20_address(stablecoin));
        assert!(B20Variant::is_b20_address(security));
        assert_eq!(B20Variant::from_address(stablecoin), Some(B20Variant::Stablecoin));
        assert_eq!(B20Variant::from_address(security), Some(B20Variant::Security));
    }

    #[test]
    fn test_create_token_deploys_ef_stub() {
        let mut storage = HashMapStorageProvider::new(1);
        let caller = Address::repeat_byte(0x55);
        let salt = B256::repeat_byte(0xAA);
        let (expected_addr, _) = B20Variant::B20.compute_address(caller, salt);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = B20FactoryStorage::new(ctx);
            let token = factory.create_b20(caller, b20_call(salt)).unwrap();

            assert_eq!(token, expected_addr);
            assert!(ctx.has_bytecode(expected_addr).unwrap());
        });
    }

    #[test]
    fn test_create_token_stores_metadata_and_uses_variant_decimals() {
        let mut storage = HashMapStorageProvider::new(1);
        let caller = Address::repeat_byte(0x55);
        let salt = B256::repeat_byte(0xBB);
        let call =
            create_call(IB20Factory::B20Variant::DEFAULT, token_params("My Token", "MYT"), salt);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = B20FactoryStorage::new(ctx);
            let token_addr = factory.create_b20(caller, call).unwrap();
            let token = B20TokenStorage::from_address(token_addr, ctx);

            assert_eq!(token.b20.name.read().unwrap(), "My Token");
            assert_eq!(token.b20.symbol.read().unwrap(), "MYT");
            assert_eq!(token.decimals().unwrap(), 18);
            assert_eq!(token.supply_cap().unwrap(), B20FactoryStorage::DEFAULT_SUPPLY_CAP);
            assert_eq!(B20Variant::decimals_of(token_addr), Some(18));
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
            IB20Factory::B20Variant::DEFAULT,
            token_params("Supply Token", "SUP"),
            salt,
        );
        call.initCalls.push(IB20::mintCall { to: recipient, amount: supply }.abi_encode().into());

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = B20FactoryStorage::new(ctx);
            let token_addr = factory.create_b20(caller, call).unwrap();
            let token = B20TokenStorage::from_address(token_addr, ctx);

            assert_eq!(token.b20.total_supply.read().unwrap(), supply);
            assert_eq!(token.balance_of(recipient).unwrap(), supply);
        });
    }

    #[test]
    fn test_create_token_init_calls_use_factory_caller_and_restore_creator() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        let creator = Address::repeat_byte(0x55);
        let spender = Address::repeat_byte(0x77);
        let salt = B256::repeat_byte(0xCE);
        let allowance = U256::from(123u64);
        let mut call = create_call(
            IB20Factory::B20Variant::DEFAULT,
            token_params("Caller Token", "CALL"),
            salt,
        );
        call.initCalls.push(IB20::approveCall { spender, amount: allowance }.abi_encode().into());
        storage.set_caller(creator);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = B20FactoryStorage::new(ctx);
            let token_addr = factory.create_b20(creator, call).unwrap();
            let token = B20TokenStorage::from_address(token_addr, ctx);

            assert_eq!(ctx.caller(), creator);
            assert_eq!(token.allowance(B20FactoryStorage::ADDRESS, spender).unwrap(), allowance);
            assert_eq!(token.allowance(creator, spender).unwrap(), U256::ZERO);
        });
    }

    #[test]
    fn test_create_token_reverts_if_salt_reused() {
        let mut storage = HashMapStorageProvider::new(1);
        let caller = Address::repeat_byte(0x55);
        let salt = B256::repeat_byte(0xEE);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = B20FactoryStorage::new(ctx);
            factory.create_b20(caller, b20_call(salt)).unwrap();
            let result = factory.create_b20(caller, b20_call(salt));
            assert!(result.is_err());
        });
    }

    #[test]
    fn test_create_token_reverts_for_invalid_version_and_variant() {
        let mut storage = HashMapStorageProvider::new(1);
        let caller = Address::repeat_byte(0x55);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = B20FactoryStorage::new(ctx);

            let mut bad_params = token_params("Bad Version", "BAD");
            bad_params.version = B20FactoryStorage::CREATE_TOKEN_VERSION + 1;
            let bad_version =
                create_call(IB20Factory::B20Variant::DEFAULT, bad_params, B256::repeat_byte(0x01));
            assert!(factory.create_b20(caller, bad_version).is_err());

            let bad_variant = IB20Factory::createB20Call {
                variant: IB20Factory::B20Variant::__Invalid,
                salt: B256::repeat_byte(0x02),
                params: token_params("Bad Variant", "BAD").abi_encode().into(),
                initCalls: Vec::new(),
            };
            assert!(factory.create_b20(caller, bad_variant).is_err());
        });
    }

    #[test]
    fn test_create_token_reverts_for_invalid_params_encoding() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        let call = IB20Factory::createB20Call {
            variant: IB20Factory::B20Variant::DEFAULT,
            salt: B256::repeat_byte(0x04),
            params: Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]),
            initCalls: Vec::new(),
        };

        StorageCtx::enter(&mut storage, |ctx| {
            let output = dispatch_factory_revert(ctx, call);
            assert!(output.starts_with(&IB20Factory::createB20Call::SELECTOR));
        });
    }

    #[test]
    fn test_create_token_allows_empty_default_name_and_symbol() {
        let mut storage = HashMapStorageProvider::new(1);
        let caller = Address::repeat_byte(0x55);
        let salt = B256::repeat_byte(0x05);
        let call = create_call(IB20Factory::B20Variant::DEFAULT, token_params("", ""), salt);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = B20FactoryStorage::new(ctx);
            let token_addr = factory.create_b20(caller, call).unwrap();
            let token = B20TokenStorage::from_address(token_addr, ctx);

            assert_eq!(token.b20.name.read().unwrap(), "");
            assert_eq!(token.b20.symbol.read().unwrap(), "");
        });
    }

    #[test]
    fn test_create_token_reverts_for_missing_stablecoin_currency() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        let params = IB20Factory::B20StablecoinCreateParams {
            version: B20FactoryStorage::CREATE_TOKEN_VERSION,
            name: "Stablecoin Token".to_string(),
            symbol: "USD".to_string(),
            initialAdmin: Address::repeat_byte(0xAB),
            currency: String::new(),
        };
        let call = IB20Factory::createB20Call {
            variant: IB20Factory::B20Variant::STABLECOIN,
            salt: B256::repeat_byte(0x06),
            params: params.abi_encode().into(),
            initCalls: Vec::new(),
        };

        StorageCtx::enter(&mut storage, |ctx| {
            assert_output(
                dispatch_factory_revert(ctx, call),
                IB20Factory::InvalidCurrency { code: String::new() }.abi_encode(),
            );
        });
    }

    #[test]
    fn test_create_token_checks_stablecoin_version_before_currency() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        let params = IB20Factory::B20StablecoinCreateParams {
            version: B20FactoryStorage::CREATE_TOKEN_VERSION + 1,
            name: "Stablecoin Token".to_string(),
            symbol: "USD".to_string(),
            initialAdmin: Address::repeat_byte(0xAB),
            currency: String::new(),
        };
        let call = IB20Factory::createB20Call {
            variant: IB20Factory::B20Variant::STABLECOIN,
            salt: B256::repeat_byte(0x07),
            params: params.abi_encode().into(),
            initCalls: Vec::new(),
        };

        StorageCtx::enter(&mut storage, |ctx| {
            assert_output(
                dispatch_factory_revert(ctx, call),
                IB20Factory::UnsupportedVersion {
                    version: B20FactoryStorage::CREATE_TOKEN_VERSION + 1,
                    variant: IB20Factory::B20Variant::STABLECOIN,
                }
                .abi_encode(),
            );
        });
    }

    #[test]
    fn test_create_token_supports_stablecoin() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);

        let stablecoin_params = IB20Factory::B20StablecoinCreateParams {
            version: B20FactoryStorage::CREATE_TOKEN_VERSION,
            name: "Stablecoin Token".to_string(),
            symbol: "USD".to_string(),
            initialAdmin: Address::repeat_byte(0xAB),
            currency: "USD".to_string(),
        };
        let stablecoin_call = IB20Factory::createB20Call {
            variant: IB20Factory::B20Variant::STABLECOIN,
            salt: B256::repeat_byte(0x08),
            params: stablecoin_params.abi_encode().into(),
            initCalls: Vec::new(),
        };

        StorageCtx::enter(&mut storage, |ctx| {
            let stablecoin_addr = IB20Factory::createB20Call::abi_decode_returns(
                dispatch_factory_success(ctx, stablecoin_call).as_ref(),
            )
            .unwrap();
            let stablecoin = B20StablecoinStorage::from_address(stablecoin_addr, ctx);
            assert_eq!(stablecoin.stablecoin.currency.read().unwrap(), "USD");
            assert_eq!(stablecoin.b20.name.read().unwrap(), "Stablecoin Token");
            assert_eq!(B20Variant::from_address(stablecoin_addr), Some(B20Variant::Stablecoin));
            assert_eq!(B20Variant::decimals_of(stablecoin_addr), Some(6));
        });
    }

    #[test]
    fn test_create_security_token_stores_isin_and_ratio() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        let caller = Address::repeat_byte(0x55);
        let salt = B256::repeat_byte(0x09);
        let (expected_addr, _) = B20Variant::Security.compute_address(caller, salt);

        let security_params = IB20Factory::B20SecurityCreateParams {
            version: B20FactoryStorage::CREATE_TOKEN_VERSION,
            name: "Security Token".to_string(),
            symbol: "SEC".to_string(),
            initialAdmin: Address::repeat_byte(0xAB),
            isin: "US0000000000".to_string(),
            minimumRedeemable: U256::ONE,
        };
        let security_call = IB20Factory::createB20Call {
            variant: IB20Factory::B20Variant::SECURITY,
            salt,
            params: security_params.abi_encode().into(),
            initCalls: Vec::new(),
        };

        storage.set_caller(caller);
        StorageCtx::enter(&mut storage, |ctx| {
            assert_output(
                dispatch_factory_success(ctx, security_call),
                IB20Factory::createB20Call::abi_encode_returns(&expected_addr),
            );
            assert!(ctx.has_bytecode(expected_addr).unwrap());

            let sec_storage = B20SecurityStorage::from_address(expected_addr, ctx);
            assert_eq!(sec_storage.b20.name.read().unwrap(), "Security Token");
            assert_eq!(sec_storage.b20.symbol.read().unwrap(), "SEC");
            assert_eq!(sec_storage.decimals().unwrap(), 6);
            assert_eq!(sec_storage.security.shares_to_tokens_ratio.read().unwrap(), U256::ZERO);
            assert_eq!(sec_storage.redeem.minimum_redeemable.read().unwrap(), U256::ONE);
            // ISIN is stored in the identifiers mapping under the raw "ISIN" key.
            assert_eq!(
                sec_storage.security.identifiers.at(&String::from("ISIN")).read().unwrap(),
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
            .push(IB20::updateNameCall { newName: "Configured".to_string() }.abi_encode().into());

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = B20FactoryStorage::new(ctx);
            let token_addr = factory.create_b20(caller, call).unwrap();
            let token = B20TokenStorage::from_address(token_addr, ctx);

            assert_eq!(token.b20.name.read().unwrap(), "Configured");
        });
    }

    #[test]
    fn test_is_b20_and_variant_prefix_before_and_after_create() {
        let mut storage = HashMapStorageProvider::new(1);
        let caller = Address::repeat_byte(0x55);
        let salt = B256::repeat_byte(0x11);
        let (addr, _) = B20Variant::B20.compute_address(caller, salt);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = B20FactoryStorage::new(ctx);
            assert!(factory.is_b20(addr).unwrap());

            let token = factory.create_b20(caller, b20_call(salt)).unwrap();
            assert!(factory.is_b20(token).unwrap());
            assert_eq!(B20Variant::from_address(token), Some(B20Variant::B20));
        });
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
    fn test_transfer_and_mint_lifecycle() {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = B20FactoryStorage::new(ctx);
            let params = token_params("Lifecycle", "LIFE");
            let token_addr = factory
                .create_b20(
                    Address::repeat_byte(0xCA),
                    create_call(IB20Factory::B20Variant::DEFAULT, params, B256::repeat_byte(0x12)),
                )
                .unwrap();

            let alice = Address::repeat_byte(0xCD);
            let bob = Address::repeat_byte(0xBB);
            let mut token = token_at(token_addr, ctx);

            token.mint(alice, alice, U256::from(1_000u64), true).unwrap();
            token.transfer(alice, bob, U256::from(300u64), false).unwrap();
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
            let mut factory = B20FactoryStorage::new(ctx);
            let first = factory
                .create_b20(Address::repeat_byte(0xCA), b20_call(B256::repeat_byte(0x07)))
                .unwrap();
            let second = factory
                .create_b20(Address::repeat_byte(0xCA), b20_call(B256::repeat_byte(0x08)))
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
        let (expected_token, _) = B20Variant::B20.compute_address(creator, salt);
        let mut call = create_call(
            IB20Factory::B20Variant::DEFAULT,
            token_params("Dispatch Token", "DSP"),
            salt,
        );
        call.initCalls.push(
            IB20::mintCall { to: Address::repeat_byte(0xCD), amount: U256::from(1_000u64) }
                .abi_encode()
                .into(),
        );
        call.initCalls.push(
            IB20::updateContractURICall { newURI: "ipfs://dispatch".to_string() }
                .abi_encode()
                .into(),
        );

        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        storage.set_caller(creator);

        StorageCtx::enter(&mut storage, |ctx| {
            assert_output(
                dispatch_factory_success(
                    ctx,
                    IB20Factory::getB20AddressCall {
                        variant: IB20Factory::B20Variant::DEFAULT,
                        sender: creator,
                        salt,
                    },
                ),
                IB20Factory::getB20AddressCall::abi_encode_returns(&expected_token),
            );
            assert_output(
                dispatch_factory_revert(
                    ctx,
                    IB20Factory::getB20AddressCall {
                        variant: IB20Factory::B20Variant::__Invalid,
                        sender: creator,
                        salt,
                    },
                ),
                IB20Factory::InvalidVariant {}.abi_encode(),
            );

            assert_output(
                dispatch_factory_success(ctx, call),
                IB20Factory::createB20Call::abi_encode_returns(&expected_token),
            );
            assert!(ctx.has_bytecode(expected_token).unwrap());

            assert_output(
                dispatch_factory_success(ctx, IB20Factory::isB20Call { token: expected_token }),
                IB20Factory::isB20Call::abi_encode_returns(&true),
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
                B20Variant::B20.compute_address(caller, B256::repeat_byte(0x09));
            assert_eq!(token_addr.as_slice()[11..], tail);
            assert!(!ctx.has_bytecode(token_addr).unwrap());

            let mut token = token_at(token_addr, ctx);
            let result = token.dispatch(ctx, &IB20::nameCall {}.abi_encode()).unwrap();

            assert!(result.is_revert());
            assert!(result.bytes.is_empty());
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
        let (token_addr, _) = B20Variant::B20.compute_address(creator, salt);
        let mut call = create_call(
            IB20Factory::B20Variant::DEFAULT,
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
                IB20Factory::createB20Call::abi_encode_returns(&token_addr),
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
    fn test_create_security_token_grants_default_admin_role() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        let caller = Address::repeat_byte(0x55);
        let initial_admin = Address::repeat_byte(0xAB);

        let params = IB20Factory::B20SecurityCreateParams {
            version: B20FactoryStorage::CREATE_TOKEN_VERSION,
            name: "Security Token".to_string(),
            symbol: "SEC".to_string(),
            initialAdmin: initial_admin,
            isin: "US0000000001".to_string(),
            minimumRedeemable: U256::ZERO,
        };
        let call = IB20Factory::createB20Call {
            variant: IB20Factory::B20Variant::SECURITY,
            salt: B256::repeat_byte(0x50),
            params: params.abi_encode().into(),
            initCalls: Vec::new(),
        };

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = B20FactoryStorage::new(ctx);
            let token_addr = factory.create_b20(caller, call).unwrap();

            let token = B20SecurityToken::with_storage_and_policy(
                B20SecurityStorage::from_address(token_addr, ctx),
                PolicyHandle::new(ctx),
            );
            assert!(token.has_role(B20TokenRole::DefaultAdmin.id(), initial_admin).unwrap());
            assert!(!token.has_role(B20TokenRole::DefaultAdmin.id(), Address::ZERO).unwrap());
        });

        // Zero initialAdmin grants no role.
        let params_no_admin = IB20Factory::B20SecurityCreateParams {
            version: B20FactoryStorage::CREATE_TOKEN_VERSION,
            name: "No Admin".to_string(),
            symbol: "NA".to_string(),
            initialAdmin: Address::ZERO,
            isin: "US0000000002".to_string(),
            minimumRedeemable: U256::ZERO,
        };
        let call_no_admin = IB20Factory::createB20Call {
            variant: IB20Factory::B20Variant::SECURITY,
            salt: B256::repeat_byte(0x51),
            params: params_no_admin.abi_encode().into(),
            initCalls: Vec::new(),
        };

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = B20FactoryStorage::new(ctx);
            let token_addr = factory.create_b20(caller, call_no_admin).unwrap();

            let token = B20SecurityToken::with_storage_and_policy(
                B20SecurityStorage::from_address(token_addr, ctx),
                PolicyHandle::new(ctx),
            );
            assert!(!token.has_role(B20TokenRole::DefaultAdmin.id(), initial_admin).unwrap());
            assert!(!token.has_role(B20TokenRole::DefaultAdmin.id(), Address::ZERO).unwrap());
        });
    }

    #[test]
    fn test_create_security_token_reverts_for_empty_isin() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);

        let params = IB20Factory::B20SecurityCreateParams {
            version: B20FactoryStorage::CREATE_TOKEN_VERSION,
            name: "Security Token".to_string(),
            symbol: "SEC".to_string(),
            initialAdmin: Address::repeat_byte(0xAB),
            isin: String::new(),
            minimumRedeemable: U256::ZERO,
        };
        let call = IB20Factory::createB20Call {
            variant: IB20Factory::B20Variant::SECURITY,
            salt: B256::repeat_byte(0x52),
            params: params.abi_encode().into(),
            initCalls: Vec::new(),
        };

        StorageCtx::enter(&mut storage, |ctx| {
            assert_output(
                dispatch_factory_revert(ctx, call),
                IB20Factory::MissingRequiredField {}.abi_encode(),
            );
        });

        // Bad version with empty ISIN reverts with UnsupportedVersion, not MissingRequiredField.
        let params_bad_version = IB20Factory::B20SecurityCreateParams {
            version: B20FactoryStorage::CREATE_TOKEN_VERSION + 1,
            name: "Security Token".to_string(),
            symbol: "SEC".to_string(),
            initialAdmin: Address::repeat_byte(0xAB),
            isin: String::new(),
            minimumRedeemable: U256::ZERO,
        };
        let call_bad_version = IB20Factory::createB20Call {
            variant: IB20Factory::B20Variant::SECURITY,
            salt: B256::repeat_byte(0x53),
            params: params_bad_version.abi_encode().into(),
            initCalls: Vec::new(),
        };

        StorageCtx::enter(&mut storage, |ctx| {
            assert_output(
                dispatch_factory_revert(ctx, call_bad_version),
                IB20Factory::UnsupportedVersion {
                    version: B20FactoryStorage::CREATE_TOKEN_VERSION + 1,
                    variant: IB20Factory::B20Variant::SECURITY,
                }
                .abi_encode(),
            );
        });
    }
}
