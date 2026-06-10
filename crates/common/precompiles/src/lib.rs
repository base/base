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
pub use spec::BasePrecompileSpec;

mod activation;
pub use activation::{
    ActivationFeature, ActivationRegistry, ActivationRegistryStorage, IActivationRegistry,
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
pub use common::{
    B20CoreStorage, B20Guards, B20PausableFeature, B20PolicyType, B20TokenRole, Burnable,
    Configurable, Eip712Domain, IB20, Mintable, Pausable, PermitArgs, Permittable, Policy,
    PolicyRegistry, RoleManaged, Token, TokenAccounting, Transferable,
};
#[cfg(any(test, feature = "test-utils"))]
pub use common::{InMemoryPolicy, InMemoryTokenAccounting, TestStablecoinToken, TestToken};

mod observer;
pub use observer::{EndGuard, NoopPrecompileCallObserver, PrecompileCallObserver};

mod metrics;
pub use metrics::{
    BerylAuxiliaryMetrics, BerylCallOutcome, BerylCallRecorder, BerylCallTimer,
    BerylErrorClassifier, BerylErrorKind, BerylMetricLabels, BerylSelector, PrecompileCallMetric,
    PrecompileCallOutcome, PrecompileCallStatus,
};

mod b20_asset;
pub use b20_asset::{
    AssetAccounting, B20AssetExtensionStorage, B20AssetInit, B20AssetPrecompile, B20AssetStorage,
    B20AssetToken, IB20Asset,
};

mod b20_stablecoin;
pub use b20_stablecoin::{
    B20StablecoinExtensionStorage, B20StablecoinInit, B20StablecoinPrecompile,
    B20StablecoinStorage, B20StablecoinToken, IB20Stablecoin, StablecoinAccounting,
};

mod b20_factory;
pub use b20_factory::{
    B20Factory, B20FactoryStorage, B20Variant, CommonParams, IB20Factory, TokenCreateParams,
};

mod policy;
pub use policy::{
    IPolicyRegistry, PackedPolicy, PolicyHandle, PolicyRegistryPrecompile, PolicyRegistryStorage,
};
