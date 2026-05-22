//! Shared business logic for all Base-native token variants.

mod ops;
pub use ops::{
    B20Guards, B20TokenRole, Burnable, Configurable, Mintable, Pausable, PermitArgs, Permittable,
    RoleManaged, Transferable,
};

mod policy;
#[cfg(any(test, feature = "test-utils"))]
pub(super) mod test_utils;
pub use policy::{Policy, PolicyRegistry};
#[cfg(any(test, feature = "test-utils"))]
pub use test_utils::{InMemoryPolicy, InMemoryTokenAccounting, TestStablecoinToken, TestToken};

mod token;
pub use token::Token;

mod token_accounting;
pub use token_accounting::TokenAccounting;
