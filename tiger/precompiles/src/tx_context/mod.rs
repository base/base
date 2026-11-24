//! Base transaction context native precompile.

mod abi;
pub use abi::ITransactionContext;

mod storage;
pub use storage::TxContextStorage;

mod dispatch;
