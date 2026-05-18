//! Native precompiles for Base-native tokens (B-20).

mod abi;
pub use abi::IDefaultToken;

mod common;
pub use common::{
    CAPABILITY_CAP_MUTABLE, CAPABILITY_PAUSABLE, Token, TokenAccounting,
    Burnable, Mintable, Pausable, Permittable, Redeemable, Configurable, Transferable,
};

mod default_token;
pub use default_token::{DEFAULT_TOKEN_ADDRESS, DefaultToken, DefaultTokenEvm, DefaultTokenStorage};
