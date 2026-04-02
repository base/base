//! Transaction candidate representation.

use std::sync::Arc;

use alloy_eips::eip4844::Blob;
use alloy_primitives::{Address, Bytes, U256};

/// Represents a candidate transaction to be submitted through the send pipeline.
///
/// When `blobs` is empty, the candidate produces a regular EIP-1559 (type-2)
/// transaction. When `blobs` is non-empty, it produces an EIP-4844 (type-3)
/// blob-carrying transaction.
///
/// Blobs are wrapped in [`Arc`] so that cloning a `TxCandidate` is cheap
/// (reference-count bump) rather than deep-copying 131 072 bytes per blob.
#[derive(Debug, Clone, Default)]
pub struct TxCandidate {
    /// Transaction calldata.
    pub tx_data: Bytes,
    /// EIP-4844 blobs; triggers blob tx when non-empty.
    ///
    /// Blobs are individually boxed to avoid 131 072-byte stack copies when
    /// moving them between collections.
    pub blobs: Arc<Vec<Box<Blob>>>,
    /// Recipient address. `None` means contract creation.
    pub to: Option<Address>,
    /// Gas limit. `0` means auto-estimate.
    pub gas_limit: u64,
    /// ETH value to send.
    pub value: U256,
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
    fn candidate_with_blobs_is_type3() {
        let candidate = TxCandidate { blobs: Arc::new(vec![Box::default()]), ..Default::default() };

        assert_eq!(candidate.blobs.len(), 1);
        // Struct-update preserves remaining defaults.
        assert!(candidate.tx_data.is_empty());
        assert!(candidate.to.is_none());
        assert_eq!(candidate.gas_limit, 0);
        assert_eq!(candidate.value, U256::ZERO);
    }

    #[test]
    fn candidate_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TxCandidate>();
    }
}
