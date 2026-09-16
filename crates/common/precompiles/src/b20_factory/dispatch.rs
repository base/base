//! ABI dispatch for the `B20Factory` precompile.
//!
//! The dispatcher owns everything that is *not* version-specific: it resolves the
//! active version from the block's hardfork (via [`FactoryVersions`]) and routes
//! `createB20`'s business logic to it. `getB20Address`, `isB20`, and
//! `isB20Initialized` are answered via version-invariant computations/pass-throughs.

use alloc::vec::Vec;

use alloy_primitives::{Address, B256, Bytes, keccak256};
use alloy_sol_types::{SolCall, SolType, SolValue, abi};
use base_common_genesis::BaseUpgrade;
use base_precompile_storage::{BasePrecompileError, PrecompileResult, Result, StorageCtx};

use crate::{
    B20FactoryStorage, B20Variant, Factory, FactoryV1, FactoryVersion, FactoryVersions,
    IB20Factory, NoopPrecompileCallObserver, PrecompileAuxiliaryMetrics, PrecompileCallObserver,
    PrecompileCallRecorder, PrecompileMetricLabels,
};

impl<'a> B20FactoryStorage<'a> {
    /// ABI-dispatches `calldata` to the appropriate `IB20Factory` handler for `upgrade`.
    pub fn dispatch(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        upgrade: BaseUpgrade,
    ) -> PrecompileResult {
        self.dispatch_with_observer(ctx, calldata, upgrade, NoopPrecompileCallObserver)
    }

    /// ABI-dispatches `calldata` to the appropriate `IB20Factory` handler with an observer.
    pub fn dispatch_with_observer<O>(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        upgrade: BaseUpgrade,
        observer: O,
    ) -> PrecompileResult
    where
        O: PrecompileCallObserver,
    {
        let mut recorder = PrecompileCallRecorder::start(
            observer.clone(),
            PrecompileMetricLabels::factory_call(calldata),
        );
        if !ctx.call_value().is_zero() {
            return recorder.record_base_error_result(
                ctx,
                BasePrecompileError::revert(IB20Factory::NonPayable {}),
            );
        }
        if let Err(error) = recorder.deduct_calldata_gas(ctx, calldata) {
            return recorder.record_base_error_result(ctx, error);
        }
        // Gate by hardfork: resolve the active version once.
        let Some(version) = FactoryVersions::from_base_upgrade(upgrade) else {
            return recorder
                .record_base_error_result(ctx, BasePrecompileError::Revert(Bytes::new()));
        };
        recorder.record_base_result(
            ctx,
            self.route(ctx, calldata, version, upgrade, observer),
            |b| b,
        )
    }

    /// Creates a token at a deterministic address derived from `(caller, variant, salt)`,
    /// pinned to factory `V1`. `upgrade` selects the policy-logic version the created token
    /// is bound to.
    pub fn create_b20(
        &mut self,
        caller: Address,
        call: IB20Factory::createB20Call,
        upgrade: BaseUpgrade,
    ) -> Result<Address> {
        let address_hash = keccak256((caller, call.salt).abi_encode());
        let request = CreateB20Request::from_call(&call);
        FactoryV1.create_b20_decoded(
            self,
            request.variant,
            request.params,
            &request.init_calls,
            address_hash,
            upgrade,
        )
    }

    /// Decodes calldata against the active wire surface and routes it to `version`'s logic.
    fn route<O>(
        &mut self,
        ctx: StorageCtx<'_>,
        calldata: &[u8],
        version: FactoryVersion,
        upgrade: BaseUpgrade,
        observer: O,
    ) -> Result<Bytes>
    where
        O: PrecompileCallObserver,
    {
        if let Some(request) = CreateB20Request::try_from_calldata(calldata, version) {
            return self.run_create_b20(ctx, version, upgrade, &observer, request);
        }

        let logic = version.implementation();
        match version.abi().decode(calldata)? {
            IB20Factory::IB20FactoryCalls::createB20(call) => self.run_create_b20(
                ctx,
                version,
                upgrade,
                &observer,
                CreateB20Request::from_call(&call),
            ),
            IB20Factory::IB20FactoryCalls::getB20Address(call) => {
                let v = B20Variant::from_abi(call.variant).expect(
                    "abi_decode_validate rejects non-canonical discriminants before dispatch",
                );
                let hash = ctx.metered_keccak256(&(call.sender, call.salt).abi_encode())?;
                let addr = v.compute_address_from_hash(hash).0;
                Ok(IB20Factory::getB20AddressCall::abi_encode_returns(&addr).into())
            }
            IB20Factory::IB20FactoryCalls::isB20(call) => {
                let result = logic.is_b20(self, call.token)?;
                Ok(IB20Factory::isB20Call::abi_encode_returns(&result).into())
            }
            IB20Factory::IB20FactoryCalls::isB20Initialized(call) => {
                let initialized = logic.is_b20_initialized(self, call.token)?;
                Ok(IB20Factory::isB20InitializedCall::abi_encode_returns(&initialized).into())
            }
        }
    }

    /// Posts a `createB20` request: resolves the token variant, computes its deterministic
    /// address, and routes creation to `version`'s logic. Shared by the borrowed fast path and
    /// the owned safety net in [`Self::route`] so both take an identical path once decoded.
    fn run_create_b20<O>(
        &mut self,
        ctx: StorageCtx<'_>,
        version: FactoryVersion,
        upgrade: BaseUpgrade,
        observer: &O,
        request: CreateB20Request<'_>,
    ) -> Result<Bytes>
    where
        O: PrecompileCallObserver,
    {
        let logic = version.implementation();
        let caller = ctx.caller();
        // Both decode paths reject non-canonical discriminants before reaching here, so
        // `from_abi` returning `None` would be an internal invariant violation.
        let variant = B20Variant::from_abi(request.variant)
            .expect("decode paths reject non-canonical discriminants before dispatch");
        let address_hash = ctx.metered_keccak256(&(caller, request.salt).abi_encode())?;
        let internal_call_count = request.init_calls.len();
        let internal_call_bytes = request.init_calls.iter().map(|c| c.len()).sum();
        let token = logic.create_b20_decoded(
            self,
            request.variant,
            request.params,
            &request.init_calls,
            address_hash,
            upgrade,
        )?;
        observer.record_internal_calls(
            &PrecompileAuxiliaryMetrics::singleton("factory", "createB20"),
            internal_call_count,
            internal_call_bytes,
        );
        observer.record_b20_created(variant.as_label());
        Ok(IB20Factory::createB20Call::abi_encode_returns(&token).into())
    }
}

/// Normalized input to [`B20FactoryStorage::run_create_b20`].
struct CreateB20Request<'a> {
    variant: IB20Factory::B20Variant,
    salt: B256,
    params: &'a [u8],
    init_calls: Vec<&'a [u8]>,
}

impl<'a> CreateB20Request<'a> {
    /// Returns a borrowed `createB20` request when `calldata` selects and validates at `version`.
    fn try_from_calldata(calldata: &'a [u8], version: FactoryVersion) -> Option<Self> {
        let selector = calldata.first_chunk::<4>().copied()?;
        if selector != IB20Factory::createB20Call::SELECTOR {
            return None;
        }
        if !version.abi().valid_selector(selector) {
            return None;
        }
        // `decode_sequence` followed by `valid_token` matches Alloy's validating call decode.
        let rest = &calldata[4..];
        let token =
            abi::decode_sequence::<<IB20Factory::createB20Call as SolCall>::Token<'a>>(rest)
                .ok()?;
        if !<<IB20Factory::createB20Call as SolCall>::Parameters<'a> as SolType>::valid_token(
            &token,
        ) {
            return None;
        }
        Some(Self {
            variant: <IB20Factory::B20Variant as SolType>::detokenize(token.0),
            salt: token.1.0,
            params: token.2.0,
            init_calls: token.3.0.iter().map(|c| c.0).collect(),
        })
    }

    /// Builds a `CreateB20Request` from an already-decoded call.
    fn from_call(call: &'a IB20Factory::createB20Call) -> Self {
        Self {
            variant: call.variant,
            salt: call.salt,
            params: call.params.as_ref(),
            init_calls: call.initCalls.iter().map(|call| call.as_ref()).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;

    use alloy_primitives::{Address, B256, Bytes, U256, address};
    use alloy_sol_types::{SolCall, SolError, SolEvent, SolValue};
    use base_common_genesis::BaseUpgrade;
    use base_precompile_storage::{
        BasePrecompileError, Handler, HashMapStorageProvider, StorageCtx,
    };

    use crate::{
        ActivationAdminConfig, ActivationFeature, ActivationRegistryStorage, AssetAccounting,
        B20AssetStorage, B20AssetToken, B20FactoryStorage, B20StablecoinStorage, B20Variant,
        FactoryVersion, IB20, IB20Factory, NoopPrecompileCallObserver, PolicyRegistryStorage,
        PolicyVersion,
    };

    const ACTIVATION_ADMIN: Address = address!("0xcb00000000000000000000000000000000000000");
    const ACTIVATION_ADMIN_CONFIG: ActivationAdminConfig =
        ActivationAdminConfig::static_fallback(Some(ACTIVATION_ADMIN));

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

    fn token_at<'a>(
        addr: Address,
        ctx: StorageCtx<'a>,
    ) -> B20AssetToken<B20AssetStorage<'a>, PolicyRegistryStorage<'a>> {
        B20AssetToken::with_storage_and_policy(
            B20AssetStorage::from_address(addr, ctx),
            PolicyRegistryStorage::new(ctx),
            PolicyVersion::V1,
        )
    }

    fn assert_output(output: Bytes, expected: impl AsRef<[u8]>) {
        assert_eq!(output.as_ref(), expected.as_ref());
    }

    fn dispatch_factory_success(ctx: StorageCtx<'_>, call: impl SolCall) -> Bytes {
        let mut factory = B20FactoryStorage::new(ctx);
        let output = factory.dispatch(ctx, &call.abi_encode(), BaseUpgrade::Beryl).unwrap();
        assert!(!output.is_revert(), "factory call reverted: {:?}", output.bytes);
        output.bytes
    }

    fn dispatch_factory_revert(ctx: StorageCtx<'_>, call: impl SolCall) -> Bytes {
        let mut factory = B20FactoryStorage::new(ctx);
        let output = factory.dispatch(ctx, &call.abi_encode(), BaseUpgrade::Beryl).unwrap();
        assert!(output.is_revert(), "factory call unexpectedly succeeded");
        output.bytes
    }

    fn dispatch_b20_success(ctx: StorageCtx<'_>, token_addr: Address, call: impl SolCall) -> Bytes {
        let mut token = token_at(token_addr, ctx);
        let output = token.dispatch(ctx, &call.abi_encode(), BaseUpgrade::Beryl).unwrap();
        assert!(!output.is_revert(), "token call reverted: {:?}", output.bytes);
        output.bytes
    }

    #[test]
    fn dispatch_rejects_call_with_nonzero_value() {
        let mut storage = HashMapStorageProvider::new(1);
        storage.set_call_value(U256::from(1u64));
        let calldata = IB20Factory::isB20Call { token: Address::ZERO }.abi_encode();

        let out = StorageCtx::enter(&mut storage, |ctx| {
            B20FactoryStorage::new(ctx).dispatch(ctx, &calldata, BaseUpgrade::Beryl)
        })
        .expect("dispatch must not fatally error");

        assert!(out.is_revert());
        assert_eq!(
            out.bytes,
            alloy_primitives::Bytes::from(IB20Factory::NonPayable {}.abi_encode())
        );
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
            assert!(output.len() > IB20Factory::createB20Call::SELECTOR.len());
        });
    }

    #[test]
    fn invalid_params_encoding_returns_selector_only_at_cobalt() {
        let mut storage = HashMapStorageProvider::new_with_storage_features(
            1,
            base_precompile_storage::StorageFeatures::Cobalt,
        );
        activate_precompiles(&mut storage);
        let call = IB20Factory::createB20Call {
            variant: IB20Factory::B20Variant::ASSET,
            salt: B256::repeat_byte(0x04),
            params: Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]),
            initCalls: Vec::new(),
        };

        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = B20FactoryStorage::new(ctx);
            let output = factory
                .dispatch(ctx, &call.abi_encode(), BaseUpgrade::Cobalt)
                .expect("dispatch must not fail fatally");

            assert!(output.is_revert());
            assert_eq!(output.bytes, Bytes::from(IB20Factory::createB20Call::SELECTOR));
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
                token.dispatch(ctx, &IB20::nameCall {}.abi_encode(), BaseUpgrade::Beryl).unwrap();

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
        // Version byte frozen by `FactoryV1` for `B20StablecoinEventParams`.
        assert_eq!(params.version, 1);
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

    fn aliased_create_b20_calldata(
        n: usize,
        tail: &[u8],
        params: Bytes,
        salt: B256,
        variant: IB20Factory::B20Variant,
    ) -> Vec<u8> {
        assert!(n >= 1, "need at least one aliased entry");
        let base = IB20Factory::createB20Call {
            variant,
            salt,
            params,
            initCalls: alloc::vec![Bytes::copy_from_slice(tail)],
        }
        .abi_encode();

        let args = &base[4..];
        let read_off = |at: usize| -> usize {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&args[at + 24..at + 32]);
            u64::from_be_bytes(buf) as usize
        };
        let write_off = |out: &mut [u8], at: usize, v: usize| {
            out[at..at + 32].fill(0);
            out[at + 24..at + 32].copy_from_slice(&(v as u64).to_be_bytes());
        };

        // Head: [variant][salt][off_params][off_initCalls]. `initCalls` is declared last, so
        // nothing follows its tail block — widening its offset table needs no other shifts.
        let off_init_calls = read_off(96);
        assert_eq!(read_off(off_init_calls), 1, "base encoding must be one element");

        // initCalls tail block (n == 1): [len=1][off0][blob..].
        let blob_off = read_off(off_init_calls + 32);
        let blob = &args[off_init_calls + 32 + blob_off..];

        let shared_elem_off = n * 32; // offset of the shared blob, relative to right after len

        let mut out = base[..4 + off_init_calls + 32].to_vec();
        out.resize(4 + off_init_calls + 32 + n * 32 + blob.len(), 0);
        let a = &mut out[4..];
        write_off(a, off_init_calls, n); // array length
        for i in 0..n {
            write_off(a, off_init_calls + 32 + i * 32, shared_elem_off);
        }
        let blob_at = off_init_calls + 32 + n * 32;
        a[blob_at..blob_at + blob.len()].copy_from_slice(blob);
        out
    }

    #[test]
    fn aliased_create_b20_redispatches_each_entry() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        let creator = Address::repeat_byte(0xCA);
        let bob = Address::repeat_byte(0xBB);
        let salt = B256::repeat_byte(0x40);
        let (token_addr, _) = B20Variant::Asset.compute_address(creator, salt);

        let params = token_params("Aliased Token", "ALS").abi_encode().into();
        let tail = IB20::mintCall { to: bob, amount: U256::ONE }.abi_encode();
        let calldata =
            aliased_create_b20_calldata(8, &tail, params, salt, IB20Factory::B20Variant::ASSET);

        storage.set_caller(creator);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = B20FactoryStorage::new(ctx);
            let out = factory.dispatch(ctx, &calldata, BaseUpgrade::Beryl).unwrap();
            assert!(!out.is_revert(), "aliased createB20 must succeed: {:?}", out.bytes);

            assert_output(
                dispatch_b20_success(ctx, token_addr, IB20::balanceOfCall { account: bob }),
                U256::from(8u64).abi_encode(),
            );
        });
    }

    #[test]
    fn aliased_create_b20_matches_non_aliased_output() {
        let creator = Address::repeat_byte(0xCA);
        let bob = Address::repeat_byte(0xBB);
        let n = 5usize;
        let salt = B256::repeat_byte(0x41);
        let params: Bytes = token_params("Parity Token", "PAR").abi_encode().into();
        let tail = IB20::mintCall { to: bob, amount: U256::ONE }.abi_encode();

        let aliased = aliased_create_b20_calldata(
            n,
            &tail,
            params.clone(),
            salt,
            IB20Factory::B20Variant::ASSET,
        );
        let honest = IB20Factory::createB20Call {
            variant: IB20Factory::B20Variant::ASSET,
            salt,
            params,
            initCalls: alloc::vec![Bytes::copy_from_slice(&tail); n],
        }
        .abi_encode();

        let run = |calldata: &[u8]| -> Bytes {
            let mut storage = HashMapStorageProvider::new(1);
            activate_precompiles(&mut storage);
            storage.set_caller(creator);
            StorageCtx::enter(&mut storage, |ctx| {
                let mut factory = B20FactoryStorage::new(ctx);
                let out = factory.dispatch(ctx, calldata, BaseUpgrade::Beryl).unwrap();
                assert!(!out.is_revert(), "createB20 must succeed: {:?}", out.bytes);
                out.bytes
            })
        };

        assert_eq!(
            run(&aliased),
            run(&honest),
            "aliased and honestly-encoded initCalls must return identical output"
        );
    }

    #[test]
    fn large_aliased_create_b20_succeeds() {
        let mut storage = HashMapStorageProvider::new(1);
        activate_precompiles(&mut storage);
        let creator = Address::repeat_byte(0xCA);
        let bob = Address::repeat_byte(0xBB);
        let salt = B256::repeat_byte(0x42);
        let (token_addr, _) = B20Variant::Asset.compute_address(creator, salt);

        let params = token_params("Large Aliased Token", "LGA").abi_encode().into();
        let tail = IB20::mintCall { to: bob, amount: U256::ONE }.abi_encode();
        let calldata =
            aliased_create_b20_calldata(1_024, &tail, params, salt, IB20Factory::B20Variant::ASSET);

        storage.set_caller(creator);
        StorageCtx::enter(&mut storage, |ctx| {
            let mut factory = B20FactoryStorage::new(ctx);
            let out = factory.dispatch(ctx, &calldata, BaseUpgrade::Beryl).unwrap();
            assert!(!out.is_revert(), "large aliased createB20 must succeed: {:?}", out.bytes);

            assert_output(
                dispatch_b20_success(ctx, token_addr, IB20::balanceOfCall { account: bob }),
                U256::from(1_024u64).abi_encode(),
            );
        });
    }

    #[test]
    fn create_b20_dispatch_matches_owned_abi_oracle() {
        let salt = B256::repeat_byte(0x50);
        let params: Bytes = token_params("Oracle Token", "ORC").abi_encode().into();
        let tail =
            IB20::mintCall { to: Address::repeat_byte(0xEE), amount: U256::ONE }.abi_encode();

        let one_element = IB20Factory::createB20Call {
            variant: IB20Factory::B20Variant::ASSET,
            salt,
            params: params.clone(),
            initCalls: alloc::vec![Bytes::copy_from_slice(&tail)],
        }
        .abi_encode();
        let multi_element = IB20Factory::createB20Call {
            variant: IB20Factory::B20Variant::ASSET,
            salt,
            params: params.clone(),
            initCalls: alloc::vec![
                Bytes::copy_from_slice(&tail),
                Bytes::copy_from_slice(&tail),
                Bytes::copy_from_slice(&tail),
            ],
        }
        .abi_encode();

        let aliased =
            aliased_create_b20_calldata(1_024, &tail, params, salt, IB20Factory::B20Variant::ASSET);

        // The `initCalls` length word sits right after the head plus the `params` blob. Locate it
        // from the head's offset word (word index 3) rather than hardcoding a position, since
        // unlike `announce`, `createB20` has another dynamic argument (`params`) ahead of it.
        let read_off = |bytes: &[u8], at: usize| -> usize {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&bytes[at + 24..at + 32]);
            u64::from_be_bytes(buf) as usize
        };
        let off_init_calls = read_off(&one_element, 4 + 96);
        let mut past_end_length = one_element.clone();
        past_end_length[4 + off_init_calls..4 + off_init_calls + 32].fill(0xff);

        let mut trailing_garbage = one_element.clone();
        trailing_garbage.extend_from_slice(&[0u8; 16]);

        // The `variant` enum sits in the first argument word (`calldata[4..36]`), right-aligned.
        // An out-of-range discriminant is the one field the fast path detokenizes, so pin that its
        // rejection matches the owned decode rather than mishandling the value.
        let mut invalid_variant = one_element.clone();
        invalid_variant[35] = 0x07;

        // Non-canonical high-order padding on the variant word: caught by validation, not strict
        // mode, so the borrowed `valid_token` and the owned `type_check` must agree.
        let mut dirty_variant_padding = one_element.clone();
        dirty_variant_padding[4] = 0xff;

        let truncated_head = IB20Factory::createB20Call::SELECTOR.to_vec();
        let no_calldata: Vec<u8> = Vec::new();

        let rows: alloc::vec::Vec<(&'static str, Vec<u8>, bool)> = alloc::vec![
            ("honest single-element valid", one_element, true),
            ("honest multi-element valid", multi_element, true),
            ("aliased offsets valid", aliased, true),
            ("length word overruns buffer", past_end_length, false),
            ("out-of-range variant discriminant", invalid_variant, false),
            ("non-canonical variant padding", dirty_variant_padding, false),
            // alloy follows absolute offsets, so bytes past the last tail get ignored. The oracle
            // accepts, and the fast path must match.
            ("trailing garbage after valid payload", trailing_garbage, true),
            ("truncated head (only selector)", truncated_head, false),
            ("no calldata at all", no_calldata, false),
        ];

        for (name, calldata, must_accept) in rows {
            // Oracle: alloy's owned validator is the ABI spec, independent of the fix. It reads
            // full calldata (selector included) since `abi_decode_validate` peels the selector.
            let oracle_accepts = IB20Factory::createB20Call::abi_decode_validate(&calldata).is_ok();
            assert_eq!(
                oracle_accepts, must_accept,
                "row `{name}`: oracle disagrees; refresh the test if the payload changed"
            );

            let mut storage = HashMapStorageProvider::new(1);
            activate_precompiles(&mut storage);
            storage.set_caller(Address::repeat_byte(0x01));
            let outcome = StorageCtx::enter(&mut storage, |ctx| {
                B20FactoryStorage::new(ctx).route(
                    ctx,
                    &calldata,
                    FactoryVersion::V1,
                    BaseUpgrade::Beryl,
                    NoopPrecompileCallObserver,
                )
            });

            assert_eq!(
                outcome.is_ok(),
                must_accept,
                "row `{name}`: accept-set disagrees with the oracle",
            );

            if let Err(err) = outcome {
                let control = FactoryVersion::V1.abi().decode(&calldata).unwrap_err();
                assert_eq!(err, control, "row `{name}`: error bytes must match owned decode");
                // Decode-time rejections target the createB20 selector. Payloads too short to
                // carry a selector hit the shared unknown-selector path instead.
                match err {
                    BasePrecompileError::AbiDecodeFailed { selector, .. } => {
                        assert_eq!(selector, IB20Factory::createB20Call::SELECTOR)
                    }
                    BasePrecompileError::UnknownFunctionSelector(_) => {}
                    other => panic!("row `{name}`: unexpected error {other:?}"),
                }
            }
        }
    }
}
