use alloy_primitives::Bytes;

use crate::OpTransaction;

/// Trait for Base transaction environments. Allows to recover the transaction encoded bytes if
/// they're available.
pub trait BaseTxEnv {
    /// Returns the encoded bytes of the transaction.
    fn encoded_bytes(&self) -> Option<&Bytes>;
}

impl<T: revm::context::Transaction> BaseTxEnv for OpTransaction<T> {
    fn encoded_bytes(&self) -> Option<&Bytes> {
        self.enveloped_tx.as_ref()
    }
}
