//! Sealed blocks and sender recovery.

use crate::{InMemorySize, Recovered, RecoveryError, SealedHeader};

mod receipt;
pub use receipt::gas_spent_by_transactions;

mod transaction;
pub use transaction::{
    InvalidTransactionError, SignedTransaction, TransactionConversionError,
    TryFromRecoveredTransactionError, recover_signers, recover_signers_unchecked,
    try_recover_signers,
};

mod block;
pub use block::{
    BlockBody, BlockHeader, BlockRecoveryError, IndexedTx, RecoveredBlock, SealedBlock,
    SealedBlockRecoveryError, SealedBlockWith, SealedOrRecoveredBlock,
};

#[cfg(all(test, feature = "std", feature = "reth-codec"))]
mod log;
#[cfg(all(test, feature = "std", feature = "reth-codec"))]
mod withdrawal;

mod error;
pub use error::{GotExpected, GotExpectedBoxed};

mod sync;
pub use sync::{LazyLock, OnceLock};

mod serde_bounds;
pub use serde_bounds::MaybeSerde;

#[cfg(feature = "dashmap")]
mod map;
#[cfg(feature = "dashmap")]
pub use map::{DashMap, DashSet, Entry, mapref};

mod ethereum_receipt;
pub use ethereum_receipt::EthereumReceiptRoot;
