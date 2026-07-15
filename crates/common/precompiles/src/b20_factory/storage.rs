use alloc::{string::ToString, vec::Vec};

use alloy_primitives::{Address, B256, Bytes, U256, address, b256, keccak256};
use alloy_sol_types::{SolCall, SolValue};
use base_precompile_macros::contract;
use base_precompile_storage::{BasePrecompileError, Result};
use revm::state::Bytecode;

use crate::{
    ActivationRegistryStorage, B20_MAX_SUPPLY_CAP, B20AssetInit, B20AssetStorage, B20AssetToken,
    B20StablecoinInit, B20StablecoinStorage, B20StablecoinToken, B20TokenRole, B20Variant,
    BerylAuxiliaryMetrics, IB20Factory, NoopPrecompileCallObserver, PolicyHandle,
    PrecompileCallObserver, Token,
};

/// Version byte for `B20StablecoinEventParams` inside `B20Created.variantParams`.
const B20_STABLECOIN_EVENT_PARAMS_VERSION: u8 = 1;

/// ABI-encodes stablecoin-specific `variantParams` for `B20Created`.
/// DEFAULT and ASSET call sites use `Bytes::new()` directly.
fn encode_stablecoin_variant_params(currency: &str) -> Bytes {
    IB20Factory::B20StablecoinEventParams {
        version: B20_STABLECOIN_EVENT_PARAMS_VERSION,
        currency: currency.to_string(),
    }
    .abi_encode()
    .into()
}

/// Maximum total supply for all newly-created B-20 tokens.
const DEFAULT_SUPPLY_CAP: U256 = B20_MAX_SUPPLY_CAP;

/// Initial multiplier storage value. Reads treat zero as WAD precision (1:1).
const INITIAL_MULTIPLIER: U256 = U256::ZERO;

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

    /// Creates a token at a deterministic address derived from `(caller, variant, salt)`.
    pub fn create_b20(
        &mut self,
        caller: Address,
        call: IB20Factory::createB20Call,
    ) -> Result<Address> {
        let address_hash = keccak256((caller, call.salt).abi_encode());
        self.create_b20_with_observer(call, address_hash, NoopPrecompileCallObserver)
    }

    /// Creates a token at a deterministic address and records observer-only metrics.
    ///
    /// `address_hash` must be `keccak256(abi_encode(caller, call.salt))`. The caller is
    /// responsible for computing this hash (and charging gas via `ctx.metered_keccak256`
    /// before calling this method from a dispatch context).
    pub fn create_b20_with_observer<O>(
        &mut self,
        call: IB20Factory::createB20Call,
        address_hash: B256,
        observer: O,
    ) -> Result<Address>
    where
        O: PrecompileCallObserver,
    {
        let variant = B20Variant::from_abi(call.variant)
            .ok_or_else(|| BasePrecompileError::revert(IB20Factory::InvalidVariant {}))?;
        ActivationRegistryStorage::new(self.storage)
            .ensure_activated(variant.activation_feature().id())?;
        let params = TokenCreateParams::decode(variant, &call.params)?;
        Self::check_version(params.version(), variant)?;
        params.validate()?;
        let (token_address, _) = variant.compute_address_from_hash(address_hash);

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
            TokenCreateParams::Stablecoin { common, init } => {
                self.init_stablecoin(token_address, common, init, init_calls, observer)?;
            }
            TokenCreateParams::Asset { common, init } => {
                self.init_asset_token(token_address, common, init, init_calls, observer)?;
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
    pub fn is_b20_initialized(&self, token: Address) -> Result<bool> {
        if !B20Variant::has_b20_prefix(token) {
            return Ok(false);
        }
        self.storage.with_account_info(token, |info| Ok(info.code_hash == FACTORY_MARKER_CODE_HASH))
    }

    fn init_stablecoin<O>(
        &mut self,
        token_address: Address,
        common: CommonParams,
        init: B20StablecoinInit,
        init_calls: Vec<Bytes>,
        observer: O,
    ) -> Result<()>
    where
        O: PrecompileCallObserver,
    {
        let mut token = B20StablecoinToken::with_storage_and_policy(
            B20StablecoinStorage::from_address(token_address, self.storage),
            PolicyHandle::new(self.storage),
        );
        let (name, symbol, currency) =
            (init.name.clone(), init.symbol.clone(), init.currency.clone());
        token.accounting_mut().initialize(init)?;

        self.emit_event(IB20Factory::B20Created {
            token: token_address,
            variant: B20Variant::Stablecoin.abi(),
            name,
            symbol,
            decimals: B20Variant::Stablecoin
                .decimals()
                .expect("stablecoin has fixed 6-decimal precision"),
            variantParams: encode_stablecoin_variant_params(&currency),
        })?;

        if !common.initial_admin.is_zero() {
            token.grant_role_unchecked(
                B20TokenRole::DefaultAdmin.id(),
                common.initial_admin,
                Self::ADDRESS,
            )?;
        }

        let internal_call_count = init_calls.len();
        let internal_call_bytes = init_calls.iter().map(|call| call.len()).sum();
        observer.record_internal_calls(
            &BerylAuxiliaryMetrics::singleton("factory", "createB20"),
            internal_call_count,
            internal_call_bytes,
        );

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

    fn init_asset_token<O>(
        &mut self,
        token_address: Address,
        common: CommonParams,
        init: B20AssetInit,
        init_calls: Vec<Bytes>,
        observer: O,
    ) -> Result<()>
    where
        O: PrecompileCallObserver,
    {
        let mut token = B20AssetToken::with_storage_and_policy(
            B20AssetStorage::from_address(token_address, self.storage),
            PolicyHandle::new(self.storage),
        );
        let (name, symbol, decimals) = (init.name.clone(), init.symbol.clone(), init.decimals);
        token.accounting_mut().initialize(init)?;

        self.emit_event(IB20Factory::B20Created {
            token: token_address,
            variant: B20Variant::Asset.abi(),
            name,
            symbol,
            decimals,
            variantParams: Bytes::new(),
        })?;

        if !common.initial_admin.is_zero() {
            token.grant_role_unchecked(
                B20TokenRole::DefaultAdmin.id(),
                common.initial_admin,
                Self::ADDRESS,
            )?;
        }

        let internal_call_count = init_calls.len();
        let internal_call_bytes = init_calls.iter().map(|call| call.len()).sum();
        observer.record_internal_calls(
            &BerylAuxiliaryMetrics::singleton("factory", "createB20"),
            internal_call_count,
            internal_call_bytes,
        );

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

    fn check_version(version: u8, variant: B20Variant) -> Result<()> {
        if version != variant.supported_version() {
            return Err(BasePrecompileError::revert(IB20Factory::UnsupportedVersion {
                version,
                variant: variant.abi(),
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
pub struct CommonParams {
    /// Token creation parameter version.
    version: u8,
    /// Initial default admin granted after token initialization.
    initial_admin: Address,
}

/// Decoded creation parameters typed per token variant.
///
/// Each arm carries a typed `init` struct that maps 1-to-1 to its storage
/// `initialize()` call, plus the shared control-flow fields in `common`.
#[derive(Debug)]
pub enum TokenCreateParams {
    /// Stablecoin B-20 token creation parameters.
    Stablecoin {
        /// Shared control-flow fields.
        common: CommonParams,
        /// Stablecoin initialization fields.
        init: B20StablecoinInit,
    },
    /// Asset B-20 token creation parameters.
    Asset {
        /// Shared control-flow fields.
        common: CommonParams,
        /// Asset-token initialization fields.
        init: B20AssetInit,
    },
}

impl TokenCreateParams {
    /// Decodes ABI-encoded creation parameters for `variant`.
    pub fn decode(variant: B20Variant, params: &Bytes) -> Result<Self> {
        match variant {
            B20Variant::Stablecoin => {
                let p = IB20Factory::B20StablecoinCreateParams::abi_decode_validate(params)
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
            B20Variant::Asset => {
                let p = IB20Factory::B20AssetCreateParams::abi_decode_validate(params)
                    .map_err(Self::invalid_params)?;
                Ok(Self::Asset {
                    common: CommonParams { version: p.version, initial_admin: p.initialAdmin },
                    init: B20AssetInit {
                        name: p.name,
                        symbol: p.symbol,
                        supply_cap: DEFAULT_SUPPLY_CAP,
                        multiplier: INITIAL_MULTIPLIER,
                        decimals: p.decimals,
                    },
                })
            }
        }
    }

    /// Returns the shared token creation parameter version.
    pub const fn version(&self) -> u8 {
        match self {
            Self::Stablecoin { common, .. } | Self::Asset { common, .. } => common.version,
        }
    }

    /// Validates variant-specific invariants after the shared version check.
    ///
    /// Each arm owns its own rules. Version is checked first by the caller (`check_version`)
    /// so that version errors always take precedence over field-level errors.
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Stablecoin { init, .. } => Self::validate_stablecoin(init),
            Self::Asset { init, .. } => Self::validate_asset(init),
        }
    }

    /// Validates stablecoin initialization fields.
    pub const fn validate_stablecoin(_init: &B20StablecoinInit) -> Result<()> {
        // Currency validation is delegated to `B20StablecoinStorage::initialize`, which rejects
        // empty values with `MissingRequiredField` and non-A-Z values with `InvalidCurrency`.
        Ok(())
    }

    /// Validates asset-token initialization fields.
    pub fn validate_asset(init: &B20AssetInit) -> Result<()> {
        if init.decimals < B20AssetStorage::MIN_DECIMALS
            || init.decimals > B20AssetStorage::MAX_DECIMALS
        {
            return Err(BasePrecompileError::revert(IB20Factory::InvalidDecimals {
                decimals: init.decimals,
            }));
        }
        Ok(())
    }

    /// Maps an ABI parameter decoding error into the factory error surface.
    pub fn invalid_params(error: impl core::fmt::Display) -> BasePrecompileError {
        BasePrecompileError::AbiDecodeFailed {
            selector: IB20Factory::createB20Call::SELECTOR,
            error: error.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;

    use alloy_primitives::{Address, B256, Bytes, U256, address};
    use alloy_sol_types::{SolCall, SolError, SolEvent, SolValue};
    use base_common_genesis::BaseUpgrade;
    use base_precompile_storage::{Handler, HashMapStorageProvider, StorageCtx};
    use revm::state::Bytecode;

    use super::FACTORY_MARKER_CODE_HASH;
    use crate::{
        ActivationAdminConfig, ActivationFeature, ActivationRegistryStorage, Asset,
        AssetAccounting, AssetV1, B20_MAX_SUPPLY_CAP, B20AssetStorage, B20AssetToken,
        B20FactoryStorage, B20StablecoinStorage, B20TokenRole, B20Variant, IB20, IB20Factory,
        PolicyHandle, Token, TokenAccounting,
    };

    /// Upgrade at which the asset precompile is active for factory dispatch tests.
    const TEST_UPGRADE: BaseUpgrade = BaseUpgrade::Beryl;

    const ACTIVATION_ADMIN: Address = address!("0xcb00000000000000000000000000000000000000");
    const ACTIVATION_ADMIN_CONFIG: ActivationAdminConfig =
        ActivationAdminConfig::static_fallback(Some(ACTIVATION_ADMIN));

    #[test]
    fn factory_address_matches_canonical_precompile_address() {
        assert_eq!(
            B20FactoryStorage::ADDRESS,
            address!("B20F000000000000000000000000000000000000")
        );
    }

    fn activate_precompiles(storage: &mut HashMapStorageProvider) {
        storage.set_caller(ACTIVATION_ADMIN);
        for key in [ActivationFeature::B20Stablecoin.id(), ActivationFeature::B20Asset.id()] {
            StorageCtx::enter(storage, |ctx| {
                ActivationRegistryStorage::new(ctx).activate(key, ACTIVATION_ADMIN_CONFIG).unwrap()
            });
        }
    }

    fn token_params(name: &str, symbol: &str) -> IB20Factory::B20AssetCreateParams {
        IB20Factory::B20AssetCreateParams {
            version: B20Variant::Asset.supported_version(),
            name: name.to_string(),
            symbol: symbol.to_string(),
            initialAdmin: Address::repeat_byte(0xAB),
            decimals: B20AssetStorage::MIN_DECIMALS,
        }
    }

    fn create_call(
        variant: IB20Factory::B20Variant,
        params: IB20Factory::B20AssetCreateParams,
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
        create_call(IB20Factory::B20Variant::ASSET, token_params("Test", "TST"), salt)
    }

    fn token_at<'a>(
        addr: Address,
        ctx: StorageCtx<'a>,
    ) -> B20AssetToken<B20AssetStorage<'a>, PolicyHandle<'a>> {
        B20AssetToken::with_storage_and_policy(
            B20AssetStorage::from_address(addr, ctx),
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
        let output = token.dispatch(ctx, &call.abi_encode(), TEST_UPGRADE).unwrap();
        assert!(!output.is_revert(), "token call reverted: {:?}", output.bytes);
        output.bytes
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
    fn test_create_token_deploys_ef_stub() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        let caller = Address::repeat_byte(0x55);
        let salt = B256::repeat_byte(0xAA);
        let (expected_addr, _) = B20Variant::Asset.compute_address(caller, salt);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = B20FactoryStorage::new(ctx);
            let token = factory.create_b20(caller, b20_call(salt)).unwrap();

            assert_eq!(token, expected_addr);
            assert!(ctx.has_bytecode(expected_addr).unwrap());
        });
    }

    #[test]
    fn test_create_token_stores_metadata_and_decimals() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        let caller = Address::repeat_byte(0x55);
        let salt = B256::repeat_byte(0xBB);
        let call =
            create_call(IB20Factory::B20Variant::ASSET, token_params("My Token", "MYT"), salt);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = B20FactoryStorage::new(ctx);
            let token_addr = factory.create_b20(caller, call).unwrap();
            let token = B20AssetStorage::from_address(token_addr, ctx);

            assert_eq!(token.b20.name.read().unwrap(), "My Token");
            assert_eq!(token.b20.symbol.read().unwrap(), "MYT");
            assert_eq!(AssetAccounting::decimals(&token).unwrap(), B20AssetStorage::MIN_DECIMALS);
            assert_eq!(B20FactoryStorage::DEFAULT_SUPPLY_CAP, B20_MAX_SUPPLY_CAP);
            assert_eq!(token.supply_cap().unwrap(), B20_MAX_SUPPLY_CAP);
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
        let mut call =
            create_call(IB20Factory::B20Variant::ASSET, token_params("Supply Token", "SUP"), salt);
        call.initCalls.push(IB20::mintCall { to: recipient, amount: supply }.abi_encode().into());

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = B20FactoryStorage::new(ctx);
            let token_addr = factory.create_b20(caller, call).unwrap();
            let token = B20AssetStorage::from_address(token_addr, ctx);

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
        let mut call =
            create_call(IB20Factory::B20Variant::ASSET, token_params("Caller Token", "CALL"), salt);
        call.initCalls.push(IB20::approveCall { spender, amount: allowance }.abi_encode().into());
        storage.set_caller(creator);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = B20FactoryStorage::new(ctx);
            let token_addr = factory.create_b20(creator, call).unwrap();
            let token = B20AssetStorage::from_address(token_addr, ctx);

            assert_eq!(ctx.caller(), creator);
            assert_eq!(token.allowance(B20FactoryStorage::ADDRESS, spender).unwrap(), allowance);
            assert_eq!(token.allowance(creator, spender).unwrap(), U256::ZERO);
        });
    }

    #[test]
    fn test_create_token_reverts_if_salt_reused() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        let caller = Address::repeat_byte(0x55);
        let salt = B256::repeat_byte(0xEE);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = B20FactoryStorage::new(ctx);
            factory.create_b20(caller, b20_call(salt)).unwrap();
            let result = factory.create_b20(caller, b20_call(salt));
            assert!(result.is_err());
        });
    }

    /// A prefunded token address (balance > 0, no code) must not block `create_b20`.
    /// The factory collision check rejects only accounts that already have code, so a
    /// prefunded address is a valid deployment target and `set_code` must succeed.
    #[test]
    fn test_create_token_at_prefunded_address_succeeds() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);

        let caller = Address::repeat_byte(0x55);
        let salt = B256::repeat_byte(0xF0);
        let (token_addr, _) = B20Variant::Asset.compute_address(caller, salt);

        storage.set_balance(token_addr, U256::from(1u64));

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = B20FactoryStorage::new(ctx);
            factory.create_b20(caller, b20_call(salt)).unwrap();
        });
    }

    #[test]
    fn test_create_token_reverts_for_invalid_version_and_variant() {
        let mut storage = HashMapStorageProvider::new(1);
        let caller = Address::repeat_byte(0x55);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = B20FactoryStorage::new(ctx);

            let mut bad_params = token_params("Bad Version", "BAD");
            bad_params.version = B20Variant::Asset.supported_version() + 1;
            let bad_version =
                create_call(IB20Factory::B20Variant::ASSET, bad_params, B256::repeat_byte(0x01));
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
    fn test_create_default_token_checks_version() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);

        let mut params = token_params("Default Token", "DEF");
        params.version = B20Variant::Asset.supported_version() + 1;
        let call = create_call(IB20Factory::B20Variant::ASSET, params, B256::repeat_byte(0x55));

        StorageCtx::enter(&mut storage, |ctx| {
            assert_output(
                dispatch_factory_revert(ctx, call),
                IB20Factory::UnsupportedVersion {
                    version: B20Variant::Asset.supported_version() + 1,
                    variant: IB20Factory::B20Variant::ASSET,
                }
                .abi_encode(),
            );
        });
    }

    #[test]
    fn test_create_token_reverts_for_invalid_params_encoding() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        let call = IB20Factory::createB20Call {
            variant: IB20Factory::B20Variant::ASSET,
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
        activate_precompiles(&mut storage);
        let caller = Address::repeat_byte(0x55);
        let salt = B256::repeat_byte(0x05);
        let call = create_call(IB20Factory::B20Variant::ASSET, token_params("", ""), salt);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = B20FactoryStorage::new(ctx);
            let token_addr = factory.create_b20(caller, call).unwrap();
            let token = B20AssetStorage::from_address(token_addr, ctx);

            assert_eq!(token.b20.name.read().unwrap(), "");
            assert_eq!(token.b20.symbol.read().unwrap(), "");
        });
    }

    #[test]
    fn test_create_token_reverts_for_missing_stablecoin_currency() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        let params = IB20Factory::B20StablecoinCreateParams {
            version: B20Variant::Stablecoin.supported_version(),
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
                IB20Factory::MissingRequiredField { field: "currency".to_string() }.abi_encode(),
            );
        });
    }

    #[test]
    fn test_create_token_reverts_for_invalid_stablecoin_currency_format() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        let params = IB20Factory::B20StablecoinCreateParams {
            version: B20Variant::Stablecoin.supported_version(),
            name: "Stablecoin Token".to_string(),
            symbol: "STB".to_string(),
            initialAdmin: Address::repeat_byte(0xAB),
            currency: "usd".to_string(), // lowercase — invalid format
        };
        let call = IB20Factory::createB20Call {
            variant: IB20Factory::B20Variant::STABLECOIN,
            salt: B256::repeat_byte(0x08),
            params: params.abi_encode().into(),
            initCalls: Vec::new(),
        };

        StorageCtx::enter(&mut storage, |ctx| {
            assert_output(
                dispatch_factory_revert(ctx, call),
                IB20Factory::InvalidCurrency { code: "usd".to_string() }.abi_encode(),
            );
        });
    }

    #[test]
    fn test_create_token_checks_stablecoin_version_before_currency() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        let params = IB20Factory::B20StablecoinCreateParams {
            version: B20Variant::Stablecoin.supported_version() + 1,
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
                    version: B20Variant::Stablecoin.supported_version() + 1,
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
            version: B20Variant::Stablecoin.supported_version(),
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
        });
    }

    #[test]
    fn test_create_asset_token_stores_decimals_and_multiplier() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        let caller = Address::repeat_byte(0x55);
        let salt = B256::repeat_byte(0x09);
        let (expected_addr, _) = B20Variant::Asset.compute_address(caller, salt);

        let asset_params = IB20Factory::B20AssetCreateParams {
            version: B20Variant::Asset.supported_version(),
            name: "Asset Token".to_string(),
            symbol: "AST".to_string(),
            initialAdmin: Address::repeat_byte(0xAB),
            decimals: 12,
        };
        let asset_call = IB20Factory::createB20Call {
            variant: IB20Factory::B20Variant::ASSET,
            salt,
            params: asset_params.abi_encode().into(),
            initCalls: Vec::new(),
        };

        storage.set_caller(caller);
        StorageCtx::enter(&mut storage, |ctx| {
            assert_output(
                dispatch_factory_success(ctx, asset_call),
                IB20Factory::createB20Call::abi_encode_returns(&expected_addr),
            );
            assert!(ctx.has_bytecode(expected_addr).unwrap());

            let asset_storage = B20AssetStorage::from_address(expected_addr, ctx);
            assert_eq!(asset_storage.b20.name.read().unwrap(), "Asset Token");
            assert_eq!(asset_storage.b20.symbol.read().unwrap(), "AST");
            assert_eq!(AssetAccounting::decimals(&asset_storage).unwrap(), 12);
            assert_eq!(asset_storage.asset.multiplier.read().unwrap(), U256::ZERO);
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
            let token = B20AssetStorage::from_address(token_addr, ctx);

            assert_eq!(token.b20.name.read().unwrap(), "Configured");
        });
    }

    #[test]
    fn test_is_b20_and_variant_prefix_before_and_after_create() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        let caller = Address::repeat_byte(0x55);
        let salt = B256::repeat_byte(0x11);
        let (addr, _) = B20Variant::Asset.compute_address(caller, salt);

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = B20FactoryStorage::new(ctx);
            assert!(factory.is_b20(addr).unwrap());

            let token = factory.create_b20(caller, b20_call(salt)).unwrap();
            assert!(factory.is_b20(token).unwrap());
            assert_eq!(B20Variant::from_address(token), Some(B20Variant::Asset));
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
    fn test_factory_marker_code_hash_constant_matches_keccak256_of_marker_byte() {
        use alloy_primitives::keccak256;
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
    fn test_transfer_and_mint_lifecycle() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = B20FactoryStorage::new(ctx);
            let params = token_params("Lifecycle", "LIFE");
            let token_addr = factory
                .create_b20(
                    Address::repeat_byte(0xCA),
                    create_call(IB20Factory::B20Variant::ASSET, params, B256::repeat_byte(0x12)),
                )
                .unwrap();

            let alice = Address::repeat_byte(0xCD);
            let bob = Address::repeat_byte(0xBB);
            let mut token = token_at(token_addr, ctx);

            AssetV1.mint(&mut token, alice, alice, U256::from(1_000u64), true).unwrap();
            AssetV1.transfer(&mut token, alice, bob, U256::from(300u64), false).unwrap();
            AssetV1.mint(&mut token, alice, alice, U256::from(200u64), true).unwrap();

            assert_eq!(token.accounting().balance_of(alice).unwrap(), U256::from(900u64));
            assert_eq!(token.accounting().balance_of(bob).unwrap(), U256::from(300u64));
            assert_eq!(token.accounting().total_supply().unwrap(), U256::from(1_200u64));
        });
    }

    #[test]
    fn test_token_identity_uses_dynamic_address() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
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
                AssetV1.eip712_domain(&first_token, ctx.chain_id()).unwrap();
            let (_, _, _, _, second_domain_address, _, _) =
                AssetV1.eip712_domain(&second_token, ctx.chain_id()).unwrap();

            assert_eq!(first_domain_address, first);
            assert_eq!(second_domain_address, second);
            assert_ne!(
                AssetV1.domain_separator(&first_token, ctx.chain_id()).unwrap(),
                AssetV1.domain_separator(&second_token, ctx.chain_id()).unwrap()
            );
        });
    }

    #[test]
    fn test_factory_dispatch_create_token_predicts_and_initializes_token() {
        let creator = Address::repeat_byte(0xCA);
        let salt = B256::repeat_byte(0x31);
        let (expected_token, _) = B20Variant::Asset.compute_address(creator, salt);
        let mut call = create_call(
            IB20Factory::B20Variant::ASSET,
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
                        variant: IB20Factory::B20Variant::ASSET,
                        sender: creator,
                        salt,
                    },
                ),
                IB20Factory::getB20AddressCall::abi_encode_returns(&expected_token),
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
                B20Variant::Asset.compute_address(caller, B256::repeat_byte(0x09));
            assert_eq!(token_addr.as_slice()[11..], tail);
            assert!(!ctx.has_bytecode(token_addr).unwrap());

            let mut token = token_at(token_addr, ctx);
            let result =
                token.dispatch(ctx, &IB20::nameCall {}.abi_encode(), TEST_UPGRADE).unwrap();

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
        let (token_addr, _) = B20Variant::Asset.compute_address(creator, salt);
        let mut call = create_call(
            IB20Factory::B20Variant::ASSET,
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
    fn test_create_asset_token_grants_default_admin_role() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        let caller = Address::repeat_byte(0x55);
        let initial_admin = Address::repeat_byte(0xAB);

        let params = IB20Factory::B20AssetCreateParams {
            version: B20Variant::Asset.supported_version(),
            name: "Asset Token".to_string(),
            symbol: "AST".to_string(),
            initialAdmin: initial_admin,
            decimals: 6,
        };
        let call = IB20Factory::createB20Call {
            variant: IB20Factory::B20Variant::ASSET,
            salt: B256::repeat_byte(0x50),
            params: params.abi_encode().into(),
            initCalls: Vec::new(),
        };

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = B20FactoryStorage::new(ctx);
            let token_addr = factory.create_b20(caller, call).unwrap();

            let token = B20AssetToken::with_storage_and_policy(
                B20AssetStorage::from_address(token_addr, ctx),
                PolicyHandle::new(ctx),
            );
            assert!(
                AssetV1.has_role(&token, B20TokenRole::DefaultAdmin.id(), initial_admin).unwrap()
            );
            assert!(
                !AssetV1.has_role(&token, B20TokenRole::DefaultAdmin.id(), Address::ZERO).unwrap()
            );
        });

        // Zero initialAdmin grants no role.
        let params_no_admin = IB20Factory::B20AssetCreateParams {
            version: B20Variant::Asset.supported_version(),
            name: "No Admin".to_string(),
            symbol: "NA".to_string(),
            initialAdmin: Address::ZERO,
            decimals: 6,
        };
        let call_no_admin = IB20Factory::createB20Call {
            variant: IB20Factory::B20Variant::ASSET,
            salt: B256::repeat_byte(0x51),
            params: params_no_admin.abi_encode().into(),
            initCalls: Vec::new(),
        };

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = B20FactoryStorage::new(ctx);
            let token_addr = factory.create_b20(caller, call_no_admin).unwrap();

            let token = B20AssetToken::with_storage_and_policy(
                B20AssetStorage::from_address(token_addr, ctx),
                PolicyHandle::new(ctx),
            );
            assert!(
                !AssetV1.has_role(&token, B20TokenRole::DefaultAdmin.id(), initial_admin).unwrap()
            );
            assert!(
                !AssetV1.has_role(&token, B20TokenRole::DefaultAdmin.id(), Address::ZERO).unwrap()
            );
        });
    }

    #[test]
    fn b20created_asset_variant_emits_empty_variant_params() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        let call = IB20Factory::createB20Call {
            variant: IB20Factory::B20Variant::ASSET,
            salt: B256::repeat_byte(0x70),
            params: IB20Factory::B20AssetCreateParams {
                version: 1,
                name: "T".to_string(),
                symbol: "T".to_string(),
                initialAdmin: Address::repeat_byte(0xAB),
                decimals: 6,
            }
            .abi_encode()
            .into(),
            initCalls: Vec::new(),
        };
        storage.set_caller(Address::repeat_byte(0x01));
        StorageCtx::enter(&mut storage, |ctx| {
            dispatch_factory_success(ctx, call);
        });
        let event = storage
            .get_events(B20FactoryStorage::ADDRESS)
            .iter()
            .find_map(|l| IB20Factory::B20Created::decode_log_data(l).ok())
            .expect("B20Created must be emitted");
        assert!(event.variantParams.is_empty(), "ASSET variantParams must be empty");
    }

    #[test]
    fn b20created_stablecoin_variant_emits_encoded_currency() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        let call = IB20Factory::createB20Call {
            variant: IB20Factory::B20Variant::STABLECOIN,
            salt: B256::repeat_byte(0x71),
            params: IB20Factory::B20StablecoinCreateParams {
                version: 1,
                name: "Stable".to_string(),
                symbol: "STB".to_string(),
                initialAdmin: Address::repeat_byte(0xAB),
                currency: "USD".to_string(),
            }
            .abi_encode()
            .into(),
            initCalls: Vec::new(),
        };
        storage.set_caller(Address::repeat_byte(0x01));
        StorageCtx::enter(&mut storage, |ctx| {
            dispatch_factory_success(ctx, call);
        });
        let event = storage
            .get_events(B20FactoryStorage::ADDRESS)
            .iter()
            .find_map(|l| IB20Factory::B20Created::decode_log_data(l).ok())
            .expect("B20Created must be emitted");
        assert!(!event.variantParams.is_empty(), "STABLECOIN variantParams must not be empty");
        let params = IB20Factory::B20StablecoinEventParams::abi_decode(&event.variantParams)
            .expect("variantParams must decode as B20StablecoinEventParams");
        assert_eq!(params.version, super::B20_STABLECOIN_EVENT_PARAMS_VERSION);
        assert_eq!(params.currency, "USD");
    }

    #[test]
    fn get_b20_address_reverts_for_invalid_variant() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        let sender = Address::repeat_byte(0x11);
        let salt = B256::repeat_byte(0xAB);

        StorageCtx::enter(&mut storage, |ctx| {
            // Strict ABI decoding rejects non-canonical enum discriminants, so an
            // out-of-range variant produces an ABI decode error rather than Address::ZERO.
            dispatch_factory_revert(
                ctx,
                IB20Factory::getB20AddressCall {
                    variant: IB20Factory::B20Variant::__Invalid,
                    sender,
                    salt,
                },
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

    #[test]
    fn b20created_asset_event_emits_token_specific_decimals() {
        // Regression: B20Created.decimals for an asset token must reflect init.decimals
        // (per-token), not any variant constant. Use 12 to distinguish from both the
        // Stablecoin fixed value (6) and the Asset MIN_DECIMALS sentinel (6).
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        let call = IB20Factory::createB20Call {
            variant: IB20Factory::B20Variant::ASSET,
            salt: B256::repeat_byte(0x72),
            params: IB20Factory::B20AssetCreateParams {
                version: 1,
                name: "Custom Decimals Asset".to_string(),
                symbol: "CDA".to_string(),
                initialAdmin: Address::repeat_byte(0xAB),
                decimals: 12,
            }
            .abi_encode()
            .into(),
            initCalls: Vec::new(),
        };
        storage.set_caller(Address::repeat_byte(0x01));
        StorageCtx::enter(&mut storage, |ctx| {
            dispatch_factory_success(ctx, call);
        });
        let event = storage
            .get_events(B20FactoryStorage::ADDRESS)
            .iter()
            .find_map(|l| IB20Factory::B20Created::decode_log_data(l).ok())
            .expect("B20Created must be emitted");
        assert_eq!(
            event.decimals, 12,
            "B20Created.decimals must equal init.decimals, not any variant constant"
        );
    }

    #[test]
    fn factory_address_hashing_is_metered_for_valid_variant() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        let sender = Address::repeat_byte(0x20);
        let salt = B256::repeat_byte(0x30);
        let (expected_asset_addr, _) = B20Variant::Asset.compute_address(sender, salt);

        StorageCtx::enter(&mut storage, |ctx| {
            // Valid variant: keccak is charged and the correct address is returned.
            assert_output(
                dispatch_factory_success(
                    ctx,
                    IB20Factory::getB20AddressCall {
                        variant: IB20Factory::B20Variant::ASSET,
                        sender,
                        salt,
                    },
                ),
                IB20Factory::getB20AddressCall::abi_encode_returns(&expected_asset_addr),
            );
        });
        // One keccak call for the valid getB20Address.
        assert_eq!(
            storage.counter_keccak256(),
            1,
            "getB20Address must call keccak256 exactly once for a valid variant"
        );

        // createB20 also meters the keccak hash for valid variants. Verify the token
        // is created at the same address that getB20Address predicted.
        storage.reset_counters();
        storage.set_caller(sender);
        StorageCtx::enter(&mut storage, |ctx| {
            let call = create_call(
                IB20Factory::B20Variant::ASSET,
                token_params("Metered Token", "MTR"),
                salt,
            );
            assert_output(
                dispatch_factory_success(ctx, call),
                IB20Factory::createB20Call::abi_encode_returns(&expected_asset_addr),
            );
        });
        assert_eq!(
            storage.counter_keccak256(),
            1,
            "createB20 must call keccak256 exactly once for a valid variant"
        );
    }
}
