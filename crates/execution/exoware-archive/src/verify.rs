//! Block verification shared by the publisher, the reader and the audit.
//!
//! The transaction root commits to the body and the OP receipt root (chain-spec aware: the
//! Regolith window strips the deposit nonce for the root only) commits to the receipts.
//! Senders are not covered by any root, so they are recovered from the signatures and compared.
//! Changesets cannot be checked without re-executing the block.

use std::sync::Arc;

use alloy_consensus::{
    EMPTY_OMMER_ROOT_HASH, Header, proofs::calculate_transaction_root,
    transaction::SignerRecoverable,
};
use alloy_primitives::{Address, B256};
use base_common_consensus::{BaseReceipt, BaseTxEnvelope};
use base_execution_chainspec::BaseChainSpec;
use base_execution_consensus::calculate_receipt_root_no_memo;

use crate::{ArchiveError, BlockBody, BlockData, Family};

/// Verifies block data against its header.
#[derive(Debug, Clone)]
pub struct BlockVerifier {
    /// Chain spec for the receipt-root rules.
    pub chain_spec: Arc<BaseChainSpec>,
}

impl BlockVerifier {
    /// Creates a verifier.
    pub const fn new(chain_spec: Arc<BaseChainSpec>) -> Self {
        Self { chain_spec }
    }

    /// Checks the body against the header's transaction root. OP blocks have no ommers and no
    /// withdrawals; a non-empty withdrawal list is rejected.
    pub fn verify_body(&self, header: &Header, body: &BlockBody) -> Result<(), ArchiveError> {
        let root = calculate_transaction_root(&body.transactions);
        if root != header.transactions_root {
            return Err(ArchiveError::unavailable(
                Family::Body,
                format!(
                    "block {} transaction root {root} does not match header {}",
                    header.number, header.transactions_root
                ),
            ));
        }
        if header.ommers_hash != EMPTY_OMMER_ROOT_HASH {
            return Err(ArchiveError::unavailable(
                Family::Header,
                format!("block {} has ommers", header.number),
            ));
        }
        if body.withdrawals.as_ref().is_some_and(|w| !w.is_empty()) {
            return Err(ArchiveError::unavailable(
                Family::Body,
                format!("block {} has withdrawals", header.number),
            ));
        }
        Ok(())
    }

    /// Checks the receipts against the header's receipt root.
    pub fn verify_receipts(
        &self,
        header: &Header,
        tx_count: usize,
        receipts: &[BaseReceipt],
    ) -> Result<(), ArchiveError> {
        if receipts.len() != tx_count {
            return Err(ArchiveError::unavailable(
                Family::Receipts,
                format!("block {} has {} receipts for {tx_count} transactions", header.number, receipts.len()),
            ));
        }
        let root = calculate_receipt_root_no_memo(receipts, self.chain_spec.as_ref(), header.timestamp);
        if root != header.receipts_root {
            return Err(ArchiveError::unavailable(
                Family::Receipts,
                format!(
                    "block {} receipt root {root} does not match header {}",
                    header.number, header.receipts_root
                ),
            ));
        }
        Ok(())
    }

    /// Checks that each sender recovers from its transaction.
    pub fn verify_senders(
        &self,
        number: u64,
        transactions: &[BaseTxEnvelope],
        senders: &[Address],
    ) -> Result<(), ArchiveError> {
        if senders.len() != transactions.len() {
            return Err(ArchiveError::unavailable(
                Family::Senders,
                format!("block {number} has {} senders for {} transactions", senders.len(), transactions.len()),
            ));
        }
        for (index, (tx, sender)) in transactions.iter().zip(senders).enumerate() {
            let recovered = tx.recover_signer().ok();
            if recovered != Some(*sender) {
                return Err(ArchiveError::unavailable(
                    Family::Senders,
                    format!("block {number} tx {index} sender {sender} recovers as {recovered:?}"),
                ));
            }
        }
        Ok(())
    }

    /// Recovers the senders of `transactions`, for nodes that pruned their sender table.
    pub fn recover_senders(
        number: u64,
        transactions: &[BaseTxEnvelope],
    ) -> Result<Vec<Address>, ArchiveError> {
        transactions
            .iter()
            .enumerate()
            .map(|(index, tx)| {
                tx.recover_signer().map_err(|e| ArchiveError::InvalidBlock {
                    block: number,
                    reason: format!("tx {index} sender recovery failed: {e}"),
                })
            })
            .collect()
    }

    /// Checks body, receipts and senders of one block.
    pub fn verify_block(&self, block: &BlockData) -> Result<(), ArchiveError> {
        self.verify_body(&block.header, &block.body)?;
        self.verify_receipts(&block.header, block.body.transactions.len(), &block.receipts)?;
        self.verify_senders(block.number(), &block.body.transactions, &block.senders)
    }

    /// Checks that `headers` (consecutive, ascending) chain by parent hash and that the last one
    /// hashes to `last_hash`. Returns each header's hash.
    pub fn verify_linkage(headers: &[Header], last_hash: B256) -> Result<Vec<B256>, ArchiveError> {
        let hashes: Vec<B256> = headers.iter().map(Header::hash_slow).collect();
        for (i, pair) in headers.windows(2).enumerate() {
            if pair[1].number != pair[0].number + 1 || pair[1].parent_hash != hashes[i] {
                return Err(ArchiveError::unavailable(
                    Family::Header,
                    format!("header {} does not link to header {}", pair[1].number, pair[0].number),
                ));
            }
        }
        if hashes.last() != Some(&last_hash) {
            return Err(ArchiveError::unavailable(
                Family::Header,
                format!("segment's last header does not hash to the marker's {last_hash}"),
            ));
        }
        Ok(hashes)
    }
}

#[cfg(test)]
mod tests {
    use base_common_consensus::DepositReceiptExt;

    use super::*;
    use crate::test_utils::ChainFixture;

    /// Every fixture block verifies; the Regolith-window blocks only verify because the root
    /// strips the deposit nonce, which the archive keeps.
    #[test]
    fn fixture_verifies_across_canyon() {
        let fixture = ChainFixture::standard();
        let verifier = fixture.verifier();
        for block in &fixture.blocks {
            verifier.verify_block(block).unwrap_or_else(|e| panic!("block {} verifies: {e}", block.number()));
        }
        let regolith = &fixture.blocks[5];
        let mut stripped = regolith.receipts.clone();
        stripped[0].as_deposit_receipt_mut().expect("deposit").deposit_nonce = None;
        let canyon_rule = alloy_consensus::proofs::ordered_trie_root_with_encoder(&regolith.receipts, |r, buf| {
            alloy_eips::eip2718::Encodable2718::encode_2718(&alloy_consensus::TxReceipt::with_bloom_ref(r), buf)
        });
        assert_ne!(canyon_rule, regolith.header.receipts_root, "a Regolith block's root excludes the nonce");
        assert_eq!(regolith.receipts[0].as_deposit_receipt().unwrap().deposit_nonce, Some(4), "archive keeps the nonce");
        let canyon = &fixture.blocks[20];
        assert_eq!(canyon.receipts[0].as_deposit_receipt().unwrap().deposit_receipt_version, Some(1), "Canyon receipt version");
    }

    /// Wrong senders, swapped transactions, altered receipts and broken links are rejected.
    #[test]
    fn tampering_is_rejected() {
        let fixture = ChainFixture::standard();
        let verifier = fixture.verifier();
        let mut block = fixture.blocks[9].clone();
        block.senders[1] = Address::with_last_byte(0x42);
        assert!(matches!(verifier.verify_block(&block), Err(ArchiveError::Unavailable { family: Family::Senders, .. })), "wrong sender");
        let mut block = fixture.blocks[9].clone();
        block.body.transactions.swap(1, 2);
        assert!(matches!(verifier.verify_block(&block), Err(ArchiveError::Unavailable { family: Family::Body, .. })), "swapped txs");
        let mut block = fixture.blocks[9].clone();
        block.receipts.swap(1, 2);
        assert!(matches!(verifier.verify_block(&block), Err(ArchiveError::Unavailable { family: Family::Receipts, .. })), "swapped receipts");
        let headers: Vec<_> = fixture.blocks[8..16].iter().map(|b| b.header.clone()).collect();
        assert_eq!(BlockVerifier::verify_linkage(&headers, fixture.blocks[15].hash()).unwrap().len(), 8, "segment links");
        assert!(BlockVerifier::verify_linkage(&headers, fixture.blocks[14].hash()).is_err(), "wrong last hash");
    }
}
