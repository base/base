//! ABI dispatch for the `B20Factory` precompile.
//!
//! The dispatcher owns everything that is *not* version-specific: it resolves the
//! active version from the block's hardfork (via [`FactoryVersions`]) and routes
//! `createB20`'s business logic to it. `getB20Address`, `isB20`, and
//! `isB20Initialized` are answered via version-invariant computations/pass-throughs.

use alloy_primitives::{Address, Bytes, keccak256};
use alloy_sol_types::{SolCall, SolValue};
use base_common_genesis::BaseUpgrade;
use base_precompile_storage::{BasePrecompileError, Result, StorageCtx};
use revm::precompile::PrecompileResult;

use crate::{
    B20FactoryStorage, B20Variant, BerylAuxiliaryMetrics, BerylCallRecorder, BerylMetricLabels,
    Factory, FactoryV1, FactoryVersion, FactoryVersions, IB20Factory, NoopPrecompileCallObserver,
    PrecompileCallObserver, macros::decode_precompile_call,
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
        let mut recorder =
            BerylCallRecorder::start(observer.clone(), BerylMetricLabels::factory_call(calldata));
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
        FactoryV1.create_b20(self, call, address_hash, upgrade)
    }

    /// Decodes calldata and routes it to `version`'s logic.
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
        let logic = version.implementation();
        match decode_precompile_call!(calldata, IB20Factory::IB20FactoryCalls) {
            IB20Factory::IB20FactoryCalls::createB20(call) => {
                let caller = ctx.caller();
                // abi_decode_validate rejects non-canonical discriminants before dispatch,
                // so from_abi returning None here would be an internal invariant violation.
                let variant = B20Variant::from_abi(call.variant).expect(
                    "abi_decode_validate rejects non-canonical discriminants before dispatch",
                );
                let address_hash = ctx.metered_keccak256(&(caller, call.salt).abi_encode())?;
                let internal_call_count = call.initCalls.len();
                let internal_call_bytes = call.initCalls.iter().map(|c| c.len()).sum();
                let token = logic.create_b20(self, call, address_hash, upgrade)?;
                observer.record_internal_calls(
                    &BerylAuxiliaryMetrics::singleton("factory", "createB20"),
                    internal_call_count,
                    internal_call_bytes,
                );
                observer.record_b20_created(variant.as_label());
                Ok(IB20Factory::createB20Call::abi_encode_returns(&token).into())
            }
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
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;

    use alloy_primitives::{Address, B256, Bytes, U256, address};
    use alloy_sol_types::{SolCall, SolError, SolEvent, SolValue};
    use base_common_genesis::BaseUpgrade;
    use base_precompile_storage::{Handler, HashMapStorageProvider, StorageCtx};

    use crate::{
        ActivationAdminConfig, ActivationFeature, ActivationRegistryStorage, AssetAccounting,
        B20AssetStorage, B20AssetToken, B20FactoryStorage, B20StablecoinStorage, B20Variant, IB20,
        IB20Factory, PolicyRegistryStorage, PolicyVersion,
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
}
