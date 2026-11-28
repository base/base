//! Shared business logic for all Base-native token variants.

mod abi;
pub use abi::{IB20, IB20V1};

mod calldata;
pub use calldata::Calldata;

mod core_storage;
pub use core_storage::{B20CoreStorage, B20TokenKind, B20TokenStorage};

mod dispatch;

mod logic;
pub use logic::{B20Logic, B20Token};

mod ops;
pub use ops::{
    B20CreditRecipient, B20CreditRecipientError, B20CreditRecipientStrategy, B20TokenRole,
    Eip712Domain, NonZeroAddress, PermitArgs, ZeroAddressError,
};

mod pausable_feature;
pub use pausable_feature::B20PausableFeature;

mod policy_type;
pub use policy_type::B20PolicyType;

#[cfg(any(test, feature = "test-utils"))]
pub(super) mod test_utils;
#[cfg(any(test, feature = "test-utils"))]
pub use test_utils::{FakePolicyAccounting, InMemoryTokenAccounting, TestStablecoinToken};

mod token_accounting;
pub use token_accounting::{B20_MAX_SUPPLY_CAP, TokenAccounting, TransferPolicyIds};

mod version;
pub use version::B20Version;
