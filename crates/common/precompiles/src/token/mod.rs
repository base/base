//! Native precompiles for Base-native tokens (B-20).

mod abi;
pub use abi::{IB20, IPolicyRegistry, ITokenFactory};

mod common;
pub use common::{
    Burnable, CAPABILITY_CAP_MUTABLE, CAPABILITY_PAUSABLE, Configurable, Mintable, Pausable,
    Permittable, Policy, Redeemable, Token, TokenAccounting, Transferable,
};

mod b20;
pub use b20::{B20Token, B20TokenPrecompile, B20TokenStorage};

mod factory;
pub use factory::{TokenFactory, TokenFactoryPrecompile, TokenVariant};

mod policy_registry;
pub use policy_registry::{POLICY_REGISTRY_ADDRESS, PolicyHandle, PolicyRegistryEvm};
