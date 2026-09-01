#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod macros;

mod provider;
pub use provider::BasePrecompiles;

mod lookup;
pub use lookup::{BerylLookup, BerylLookupWithObserver};

mod spec;
pub use spec::{BasePrecompileSpec, UpgradeGatedStorageFeatures};

mod activation;
pub use activation::{
    ActivationAdminConfig, ActivationFeature, ActivationRegistry, ActivationRegistryStorage,
    IActivationRegistry,
};

mod bn254_pair;
pub use bn254_pair::{
    GRANITE, GRANITE_MAX_INPUT_SIZE, JOVIAN, JOVIAN_MAX_INPUT_SIZE, run_pair_granite,
    run_pair_jovian,
};

mod bls12_381;
pub use bls12_381::{
    ISTHMUS_G1_MSM, ISTHMUS_G1_MSM_MAX_INPUT_SIZE, ISTHMUS_G2_MSM, ISTHMUS_G2_MSM_MAX_INPUT_SIZE,
    ISTHMUS_PAIRING, ISTHMUS_PAIRING_MAX_INPUT_SIZE, JOVIAN_G1_MSM, JOVIAN_G1_MSM_MAX_INPUT_SIZE,
    JOVIAN_G2_MSM, JOVIAN_G2_MSM_MAX_INPUT_SIZE, JOVIAN_PAIRING, JOVIAN_PAIRING_MAX_INPUT_SIZE,
    run_isthmus_g1_msm, run_isthmus_g2_msm, run_isthmus_pairing, run_jovian_g1_msm,
    run_jovian_g2_msm, run_jovian_pairing,
};

mod common;
#[cfg(any(test, feature = "test-utils"))]
pub use common::{
    AbiFingerprint, FakePolicyAccounting, InMemoryTokenAccounting, TestStablecoinToken,
};
pub use common::{
    B20_MAX_SUPPLY_CAP, B20Abi, B20CoreStorage, B20Guards, B20PausableFeature, B20PolicyType,
    B20TokenRole, Eip712Domain, IB20, IB20V1, IB20V2, NonZeroAddress, PermitArgs, Token,
    TokenAccounting, TransferPolicyIds, ZeroAddressError,
};

mod observer;
pub use observer::{EndGuard, NoopPrecompileCallObserver, PrecompileCallObserver};

mod metrics;
pub use metrics::{
    BerylAuxiliaryMetrics, BerylCallOutcome, BerylCallRecorder, BerylCallTimer,
    BerylErrorClassifier, BerylErrorKind, BerylMetricLabels, BerylSelector, CALLDATA_WORD_GAS,
    PrecompileCallMetric, PrecompileCallOutcome, PrecompileCallStatus,
};

mod b20_asset;
pub(crate) use b20_asset::AssetCall;
pub use b20_asset::{
    Asset, AssetAbi, AssetAbiPair, AssetAccounting, AssetV1, AssetV2, AssetVersion, AssetVersions,
    B20AssetExtensionStorage, B20AssetInit, B20AssetPrecompile, B20AssetStorage, B20AssetToken,
    ERC165_INTERFACE_ID, ERC8056_INTERFACE_IDS, IB20Asset, IB20AssetV1, IB20AssetV2,
};

mod b20_stablecoin;
pub use b20_stablecoin::{
    B20StablecoinExtensionStorage, B20StablecoinInit, B20StablecoinPrecompile,
    B20StablecoinStorage, B20StablecoinToken, IB20Stablecoin, Stablecoin, StablecoinAccounting,
    StablecoinV1, StablecoinV2, StablecoinVersion, StablecoinVersions,
};

mod b20_factory;
pub use b20_factory::{
    B20Factory, B20FactoryStorage, B20Variant, CommonParams, Factory, FactoryAbi, FactoryV1,
    FactoryVersion, FactoryVersions, IB20Factory, IB20FactoryV1, TokenCreateParams,
};

mod policy;
pub use policy::{
    IPolicyRegistry, IPolicyRegistryV1, IPolicyRegistryV2, PackedPolicy, PolicyAbi,
    PolicyAccounting, PolicyRegistryLogic, PolicyRegistryPrecompile, PolicyRegistryStorage,
    PolicyRegistryV1, PolicyRegistryV2, PolicyVersion, PolicyVersions,
};

mod tx_context;
pub use tx_context::{ITransactionContext, TxContext, TxContextStorage};

mod nonce;
pub use nonce::{INonceManager, NonceManager, NonceManagerStorage};
