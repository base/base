//! Native precompiles for Base-native tokens (B-20).

mod abi;
pub use abi::{IB20, ITokenFactory};

mod common;
pub use common::{
    Burnable, CAPABILITY_CAP_MUTABLE, CAPABILITY_PAUSABLE, Configurable, Mintable, Pausable,
    Permittable, Redeemable, Token, TokenAccounting, Transferable,
};

mod b20;
pub use b20::{B20_TOKEN_ADDRESS, B20Token, B20TokenPrecompile, B20TokenStorage};

mod factory;
pub use factory::{TokenFactory, TokenFactoryPrecompile, TokenVariant};
