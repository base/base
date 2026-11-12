//! Verification of node payloads before they are written to the archive (TDD §4).
//!
//! Every block must chain to its parent by hash, carry transaction and receipt roots matching
//! its header, and list senders that recover from the transaction signatures. Block hashes are
//! always computed locally from the header. Changesets cannot be checked against headers
//! without re-executing the block, so they are accepted as served (see the README).

use std::sync::Arc;

use alloy_consensus::{proofs::calculate_transaction_root, transaction::SignerRecoverable};
use alloy_primitives::{Address, B256};
use base_execution_chainspec::BaseChainSpec;
use base_execution_consensus::calculate_receipt_root_no_memo;

use crate::{ArchiveBlock, BlockData, HistoryStoreError};

/// Why a node payload was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    /// The node returned a different block than requested.
    #[error("expected block {expected}, got {actual}")]
    NotContiguous {
        /// Block number expected at this position.
        expected: u64,
        /// Block number found.
        actual: u64,
    },
    /// The wire block number disagrees with its header.
    #[error("block {claimed} carries header for block {header}")]
    NumberMismatch {
        /// Number on the wire.
        claimed: u64,
        /// Number in the header.
        header: u64,
    },
    /// The parent hash does not match the previous block's hash.
    #[error("block {block} parent hash {actual} does not match previous hash {expected}")]
    BadParent {
        /// Block number.
        block: u64,
        /// Hash of the previous block.
        expected: B256,
        /// Parent hash in the header.
        actual: B256,
    },
    /// The transaction root does not match the header.
    #[error("block {block} transaction root {actual} does not match header {expected}")]
    TxRoot {
        /// Block number.
        block: u64,
        /// Root in the header.
        expected: B256,
        /// Root computed from the transactions.
        actual: B256,
    },
    /// The receipt root does not match the header.
    #[error("block {block} receipt root {actual} does not match header {expected}")]
    ReceiptRoot {
        /// Block number.
        block: u64,
        /// Root in the header.
        expected: B256,
        /// Root computed from the receipts.
        actual: B256,
    },
    /// A sender does not recover from its transaction signature.
    #[error("block {block} tx {index} sender {claimed} does not match recovered {recovered:?}")]
    SenderMismatch {
        /// Block number.
        block: u64,
        /// Transaction index.
        index: usize,
        /// Sender the node returned.
        claimed: Address,
        /// Sender recovered from the signature, if recovery succeeded.
        recovered: Option<Address>,
    },
    /// Row counts disagree within a block.
    #[error("block {block} has {actual} {what}, expected {expected}")]
    CountMismatch {
        /// Block number.
        block: u64,
        /// Which rows.
        what: &'static str,
        /// Count implied by the transactions or body indices.
        expected: u64,
        /// Count found.
        actual: u64,
    },
    /// Global transaction numbering is not contiguous.
    #[error("block {block} first tx number {actual}, expected {expected}")]
    TxNumGap {
        /// Block number.
        block: u64,
        /// Next transaction number after the previous block.
        expected: u64,
        /// First transaction number claimed.
        actual: u64,
    },
}

/// Hash and next transaction number of the block preceding a chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainTip {
    /// Number of the preceding block.
    pub number: u64,
    /// Hash of the preceding block.
    pub hash: B256,
    /// First transaction number of the block after it.
    pub next_tx_num: u64,
}

impl ChainTip {
    /// Returns the tip after `block`.
    pub const fn after(block: &BlockData) -> Self {
        Self {
            number: block.header.number,
            hash: block.hash,
            next_tx_num: block.body_indices.first_tx_num + block.body_indices.tx_count,
        }
    }
}

/// Verifies chunks of node payload against their headers.
#[derive(Debug, Clone)]
pub struct ChunkVerifier {
    /// Chain spec used for the receipt root rules.
    pub chain_spec: Arc<BaseChainSpec>,
}

impl ChunkVerifier {
    /// Creates a verifier for the given chain.
    pub const fn new(chain_spec: Arc<BaseChainSpec>) -> Self {
        Self { chain_spec }
    }

    /// Decodes and verifies a chunk that must start at `first`, after `tip` (`None` only when
    /// `first` is the genesis block).
    pub fn verify(
        &self,
        first: u64,
        mut tip: Option<ChainTip>,
        blocks: &[ArchiveBlock],
    ) -> Result<Vec<BlockData>, HistoryStoreError> {
        let mut out = Vec::with_capacity(blocks.len());
        for (offset, wire) in blocks.iter().enumerate() {
            let expected = first + offset as u64;
            if wire.number != expected {
                return Err(VerifyError::NotContiguous { expected, actual: wire.number }.into());
            }
            let block = wire.decode()?;
            self.verify_block(&block, wire.number, tip)?;
            tip = Some(ChainTip::after(&block));
            out.push(block);
        }
        Ok(out)
    }

    /// Verifies one decoded block against the previous tip.
    pub fn verify_block(
        &self,
        block: &BlockData,
        claimed: u64,
        tip: Option<ChainTip>,
    ) -> Result<(), VerifyError> {
        let number = block.number();
        if number != claimed {
            return Err(VerifyError::NumberMismatch { claimed, header: number });
        }
        if let Some(tip) = tip {
            if block.header.parent_hash != tip.hash {
                return Err(VerifyError::BadParent {
                    block: number,
                    expected: tip.hash,
                    actual: block.header.parent_hash,
                });
            }
            if block.body_indices.first_tx_num != tip.next_tx_num {
                return Err(VerifyError::TxNumGap {
                    block: number,
                    expected: tip.next_tx_num,
                    actual: block.body_indices.first_tx_num,
                });
            }
        }
        let tx_count = block.transactions.len() as u64;
        for (what, actual) in [
            ("body index transactions", block.body_indices.tx_count),
            ("senders", block.senders.len() as u64),
            ("receipts", block.receipts.len() as u64),
        ] {
            if actual != tx_count {
                return Err(VerifyError::CountMismatch {
                    block: number,
                    what,
                    expected: tx_count,
                    actual,
                });
            }
        }
        let tx_root = calculate_transaction_root(&block.transactions);
        if tx_root != block.header.transactions_root {
            return Err(VerifyError::TxRoot {
                block: number,
                expected: block.header.transactions_root,
                actual: tx_root,
            });
        }
        let receipt_root = calculate_receipt_root_no_memo(
            &block.receipts,
            self.chain_spec.as_ref(),
            block.header.timestamp,
        );
        if receipt_root != block.header.receipts_root {
            return Err(VerifyError::ReceiptRoot {
                block: number,
                expected: block.header.receipts_root,
                actual: receipt_root,
            });
        }
        for (index, (tx, claimed)) in block.transactions.iter().zip(&block.senders).enumerate() {
            let recovered = tx.recover_signer().ok();
            if recovered != Some(*claimed) {
                return Err(VerifyError::SenderMismatch {
                    block: number,
                    index,
                    claimed: *claimed,
                    recovered,
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Bytes;
    use base_common_consensus::BaseReceipt;

    use super::*;
    use crate::test_utils::ChainFixture;

    /// Returns a verifier and a 9-block fixture.
    fn setup() -> (ChunkVerifier, ChainFixture) {
        let fixture = ChainFixture::new(9, 2);
        (ChunkVerifier::new(fixture.chain_spec.clone()), fixture)
    }

    /// A correct chunk verifies and decodes to the original blocks.
    #[test]
    fn valid_chunk_verifies() {
        let (verifier, fixture) = setup();
        let tip = ChainTip::after(&fixture.blocks[3]);
        let blocks = verifier.verify(4, Some(tip), &fixture.wire(4, 8)).unwrap();
        assert_eq!(blocks, fixture.blocks[4..9].to_vec(), "decoded blocks equal the originals");
        let genesis = verifier.verify(0, None, &fixture.wire(0, 3)).unwrap();
        assert_eq!(genesis.len(), 4, "genesis chunk verifies without a tip");
    }

    /// A chunk that does not chain to the previous hash is rejected.
    #[test]
    fn bad_parent_is_rejected() {
        let (verifier, fixture) = setup();
        let tip = ChainTip { hash: B256::with_last_byte(9), ..ChainTip::after(&fixture.blocks[3]) };
        let err = verifier.verify(4, Some(tip), &fixture.wire(4, 8)).unwrap_err();
        assert!(
            matches!(
                err,
                HistoryStoreError::Verify(VerifyError::BadParent { block: 4, expected, .. })
                    if expected == B256::with_last_byte(9)
            ),
            "bad parent must be rejected at block 4, got {err}"
        );
    }

    /// A tampered transaction breaks the transaction root.
    #[test]
    fn bad_tx_root_is_rejected() {
        let (verifier, fixture) = setup();
        let mut wire = fixture.wire(4, 8);
        wire[1].transactions.swap(0, 1);
        wire[1].senders.swap(0, 1);
        let err = verifier.verify(4, Some(ChainTip::after(&fixture.blocks[3])), &wire).unwrap_err();
        assert!(
            matches!(err, HistoryStoreError::Verify(VerifyError::TxRoot { block: 5, .. })),
            "reordered transactions must fail the tx root at block 5, got {err}"
        );
    }

    /// A tampered receipt breaks the receipt root.
    #[test]
    fn bad_receipt_root_is_rejected() {
        let (verifier, fixture) = setup();
        let mut wire = fixture.wire(4, 8);
        let mut receipt: BaseReceipt = fixture.blocks[6].receipts[0].clone();
        if let BaseReceipt::Eip1559(inner) = &mut receipt {
            inner.cumulative_gas_used += 1;
        }
        wire[2].receipts[0] = ArchiveBlock::compact_bytes(&receipt);
        let err = verifier.verify(4, Some(ChainTip::after(&fixture.blocks[3])), &wire).unwrap_err();
        assert!(
            matches!(err, HistoryStoreError::Verify(VerifyError::ReceiptRoot { block: 6, .. })),
            "altered receipt must fail the receipt root at block 6, got {err}"
        );
    }

    /// A wrong sender is rejected even though the roots match.
    #[test]
    fn sender_mismatch_is_rejected() {
        let (verifier, fixture) = setup();
        let mut wire = fixture.wire(4, 8);
        wire[0].senders[1] = Address::with_last_byte(0x42);
        let err = verifier.verify(4, Some(ChainTip::after(&fixture.blocks[3])), &wire).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "verification failed: block 4 tx 1 sender {} does not match recovered Some({:?})",
                Address::with_last_byte(0x42),
                fixture.blocks[4].senders[1]
            ),
            "wrong sender must be rejected with both addresses"
        );
    }

    /// Gaps, wrong numbers and malformed rows are rejected.
    #[test]
    fn shape_errors_are_rejected() {
        let (verifier, fixture) = setup();
        let tip = ChainTip::after(&fixture.blocks[3]);
        let err = verifier.verify(4, Some(tip), &fixture.wire(5, 8)).unwrap_err();
        assert!(
            matches!(
                err,
                HistoryStoreError::Verify(VerifyError::NotContiguous { expected: 4, actual: 5 })
            ),
            "missing block must be rejected, got {err}"
        );
        let mut wire = fixture.wire(4, 8);
        wire[0].body_indices.first_tx_num += 1;
        let err = verifier.verify(4, Some(tip), &wire).unwrap_err();
        assert!(
            matches!(err, HistoryStoreError::Verify(VerifyError::TxNumGap { block: 4, .. })),
            "tx number gap must be rejected, got {err}"
        );
        let mut wire = fixture.wire(4, 8);
        wire[0].receipts[0] = Bytes::from_static(&[0xff]);
        let err = verifier.verify(4, Some(tip), &wire).unwrap_err();
        assert!(
            matches!(err, HistoryStoreError::Decode { what: "receipt", block: 4, .. }),
            "malformed receipt row must be a decode error, got {err}"
        );
    }
}
