//! Transaction candidate representation.

use std::sync::Arc;

use alloy_eips::eip4844::Blob;
use alloy_primitives::{Address, Bytes, U256};

/// Represents a candidate transaction to be submitted through the send pipeline.
///
/// When `blobs` is empty, the candidate produces a regular EIP-1559 (type-2)
/// transaction. When `blobs` is non-empty, it produces an EIP-4844 (type-3)
/// blob-carrying transaction.
#[derive(Debug, Clone, Default)]
pub struct TxCandidate {
    /// Transaction calldata.
    pub tx_data: Bytes,
    /// EIP-4844 blobs; triggers blob tx when non-empty.
    ///
    /// Wrapped in [`Arc`] for cheap cloning; individually boxed to keep
    /// 131 072-byte blobs off the stack.
    pub blobs: Arc<[Box<Blob>]>,
    /// Recipient address. `None` means contract creation.
    pub to: Option<Address>,
    /// Gas limit floor. The signed gas limit is the greater of this value, any
    /// replacement floor, and the provider estimate; `0` defers to the estimate.
    pub gas_limit: u64,
    /// ETH value to send.
    pub value: U256,
}

impl TxCandidate {
    /// Builds a zero-value self-transfer used to cancel a stuck nonce.
    ///
    /// The blobs of the transaction being cancelled are preserved: a cancel for
    /// a blob transaction must itself remain a blob transaction so the backend
    /// accepts it as a replacement.
    pub const fn cancel(sender: Address, blobs: Arc<[Box<Blob>]>) -> Self {
        Self { tx_data: Bytes::new(), blobs, to: Some(sender), gas_limit: 0, value: U256::ZERO }
    }

    /// Returns `true` when this candidate carries blobs (EIP-4844 type-3 tx).
    pub fn is_blob(&self) -> bool {
        !self.blobs.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_produces_type2_candidate() {
        let candidate = TxCandidate::default();

        assert!(candidate.tx_data.is_empty());
        assert!(candidate.blobs.is_empty());
        assert!(candidate.to.is_none());
        assert_eq!(candidate.gas_limit, 0);
        assert_eq!(candidate.value, U256::ZERO);
    }

    #[test]
    fn candidate_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TxCandidate>();
    }

    #[test]
    fn cancel_is_a_zero_value_self_transfer_that_keeps_blobs() {
        let sender = Address::with_last_byte(7);
        let candidate = TxCandidate::cancel(sender, Arc::from(vec![Box::default()]));

        assert_eq!(candidate.to, Some(sender));
        assert_eq!(candidate.value, U256::ZERO);
        assert!(candidate.tx_data.is_empty());
        assert!(candidate.is_blob());
    }
}
