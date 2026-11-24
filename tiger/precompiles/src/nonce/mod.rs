//! Base transaction 2D nonce manager native precompile.

mod abi;
pub use abi::INonceManager;

mod storage;
pub use storage::NonceManagerStorage;

mod dispatch;
