//! Shared business logic for all Base-native token variants.

mod ops;
pub use ops::{
    B20Guards, B20TokenRole, Burnable, Configurable, Eip712Domain, Mintable, Pausable,
    PermitArgs, Permittable, PolicyManaged, RoleManaged, Transferable,
};

mod policy;
pub use policy::{Policy, PolicyRegistry};

mod policy_type;
pub use policy_type::B20PolicyType;

#[cfg(any(test, feature = "test-utils"))]
pub(super) mod test_utils;
#[cfg(any(test, feature = "test-utils"))]
pub use test_utils::{InMemoryPolicy, InMemoryTokenAccounting, TestStablecoinToken, TestToken};

mod token;
pub use token::Token;

mod token_accounting;
pub use token_accounting::TokenAccounting;
