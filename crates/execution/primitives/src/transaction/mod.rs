//! Optimism transaction types

mod tx_type;

/// Kept for consistency tests
#[cfg(test)]
mod signed;

pub use base_alloy_consensus::{OpTransaction, OpTxType, OpTypedTransaction};

/// Signed transaction.
pub type OpTransactionSigned = base_alloy_consensus::OpTxEnvelope;
