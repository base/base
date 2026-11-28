#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

extern crate alloc;

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
pub use common::{FakePolicyAccounting, InMemoryTokenAccounting, TestStablecoinToken};
pub use common::{
    B20_MAX_SUPPLY_CAP, B20CoreStorage, B20CreditRecipient, B20CreditRecipientError,
    B20CreditRecipientStrategy, B20Logic, B20PausableFeature, B20PolicyType, B20Token,
    B20TokenKind, B20TokenRole, B20TokenStorage, B20Version, Calldata, Eip712Domain, IB20, IB20V1,
    NonZeroAddress, PermitArgs, TokenAccounting, TransferPolicyIds, ZeroAddressError,
};

mod b20_asset;
pub use b20_asset::{
    AssetAccounting, B20AssetExtensionStorage, B20AssetInit, B20AssetStorage, ERC165_INTERFACE_ID,
    ERC8056_INTERFACE_IDS, IB20Asset,
};

mod b20_stablecoin;
pub use b20_stablecoin::{
    B20StablecoinExtensionStorage, B20StablecoinInit, B20StablecoinStorage, IB20Stablecoin,
    StablecoinAccounting,
};

mod b20_factory;
pub use b20_factory::{
    B20FactoryStorage, B20Variant, CommonParams, FactoryVersion, FactoryVersions, IB20Factory,
    TokenCreateParams,
};

mod policy;
pub use policy::{
    IPolicyRegistry, IPolicyRegistryV1, PackedPolicy, PolicyAccounting, PolicyRegistry,
    PolicyRegistryStorage, PolicyVersion, PolicyVersions,
};

mod tx_context;
pub use tx_context::{ITransactionContext, TxContextStorage};

mod nonce;
pub use nonce::{INonceManager, NonceManagerStorage};
