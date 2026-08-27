//! EIP-8130 2D nonce manager native precompile.

mod abi;
pub use abi::INonceManager;

mod storage;
pub use storage::{NonceManagerStorage, SequenceNonceRead};

mod dispatch;

mod precompile;
pub use precompile::NonceManager;
