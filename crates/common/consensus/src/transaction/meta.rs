//! Commonly used types that contain metadata about a transaction.

use alloy_consensus::transaction::TransactionInfo;

/// Additional receipt metadata required for deposit transactions.
///
/// These fields are used to provide additional context for deposit transactions in RPC responses
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct DepositInfo {
    /// Nonce for deposit transactions. Only present in RPC responses.
    pub deposit_nonce: Option<u64>,
    /// Deposit receipt version for deposit transactions post-canyon
    pub deposit_receipt_version: Option<u64>,
}

/// Additional fields in the context of a block that contains this transaction and its deposit
/// metadata if the transaction is a deposit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BaseTransactionInfo {
    /// Additional transaction information.
    pub inner: TransactionInfo,
    /// Additional metadata for deposit transactions.
    pub deposit_meta: DepositInfo,
}

impl BaseTransactionInfo {
    /// Creates a new [`BaseTransactionInfo`] with the given [`TransactionInfo`] and
    /// [`DepositInfo`].
    pub const fn new(inner: TransactionInfo, deposit_meta: DepositInfo) -> Self {
        Self { inner, deposit_meta }
    }
}
