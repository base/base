//! Shared business logic for all Base-native token variants.

mod abi;
pub use abi::{B20Abi, IB20, IB20V1, IB20V2};

#[cfg(any(test, feature = "test-utils"))]
mod abi_fingerprint;
#[cfg(any(test, feature = "test-utils"))]
pub use abi_fingerprint::AbiFingerprint;

mod calldata_gas;
pub use calldata_gas::CalldataGas;

mod core_storage;

mod dispatch;

mod logic;
pub use logic::{B20Logic, B20Token, B20Version};
pub use core_storage::B20CoreStorage;

mod ops;
pub use ops::{
    B20CreditRecipient, B20CreditRecipientError, B20CreditRecipientStrategy, B20Guards,
    B20TokenRole, Eip712Domain, NonZeroAddress, PermitArgs, ZeroAddressError,
};

mod pausable_feature;
pub use pausable_feature::B20PausableFeature;

mod policy_type;
pub use policy_type::B20PolicyType;

#[cfg(any(test, feature = "test-utils"))]
pub(super) mod test_utils;
#[cfg(any(test, feature = "test-utils"))]
pub use test_utils::{FakePolicyAccounting, InMemoryTokenAccounting, TestStablecoinToken};

mod token;
pub use token::Token;

mod token_accounting;
pub use token_accounting::{B20_MAX_SUPPLY_CAP, TokenAccounting, TransferPolicyIds};
