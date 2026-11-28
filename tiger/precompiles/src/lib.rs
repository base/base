#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

extern crate alloc;

mod macros;

mod native;
pub use native::{NativePrecompile, NativePrecompiles};

mod spec;
pub use spec::UpgradeGatedStorageFeatures;

mod activation;
pub use activation::{
    ActivationAdminConfig, ActivationFeature, ActivationRegistryStorage, IActivationRegistry,
};

mod common;
#[cfg(any(test, feature = "test-utils"))]
pub use common::{
    AbiFingerprint, FakePolicyAccounting, InMemoryTokenAccounting, TestStablecoinToken,
};
pub use common::{
    B20_MAX_SUPPLY_CAP, B20Abi, B20CoreStorage, B20CreditRecipient, B20CreditRecipientError,
    B20Logic, B20Token, B20Version,
    B20CreditRecipientStrategy, B20Guards, B20PausableFeature, B20PolicyType, B20TokenRole,
    CalldataGas, Eip712Domain, IB20, IB20V1, IB20V2, NonZeroAddress, PermitArgs, Token,
    TokenAccounting, TransferPolicyIds, ZeroAddressError,
};

mod b20_asset;
pub(crate) use b20_asset::AssetCall;
pub use b20_asset::{
    AssetAbi, AssetAbiPair, AssetAccounting, B20AssetExtensionStorage, B20AssetInit,
    B20AssetStorage, ERC165_INTERFACE_ID, ERC8056_INTERFACE_IDS, IB20Asset, IB20AssetV1,
    IB20AssetV2,
};

mod b20_stablecoin;
pub use b20_stablecoin::{
    B20StablecoinExtensionStorage, B20StablecoinInit, B20StablecoinStorage, IB20Stablecoin,
    StablecoinAccounting,
};

mod b20_factory;
pub use b20_factory::{
    B20FactoryStorage, B20Variant, CommonParams, Factory, FactoryAbi, FactoryV1, FactoryVersion,
    FactoryVersions, IB20Factory, IB20FactoryV1, InitCalls, TokenCreateParams,
};

mod policy;
pub use policy::{
    IPolicyRegistry, IPolicyRegistryV1, IPolicyRegistryV2, IPolicyRegistryV3, PackedPolicy,
    PolicyAbi, PolicyAccounting, PolicyRegistry, PolicyRegistryStorage, PolicyVersion,
    PolicyVersions,
};

mod tx_context;
pub use tx_context::{ITransactionContext, TxContextStorage};

mod nonce;
pub use nonce::{INonceManager, NonceManagerStorage};
