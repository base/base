//! Block verification shared by the publisher, the reader and the audit.
//!
//! The transaction root commits to the body and the OP receipt root (chain-spec aware: the
//! Regolith window strips the deposit nonce for the root only) commits to the receipts.
//! Senders are not covered by any root, so they are recovered from the signatures and compared.
//! In the Regolith window the receipt root does not cover the deposit nonce, so each deposit
//! nonce is checked against its sender's before-nonce in the account changeset.
//! Changesets cannot be checked without re-executing the block.

use std::sync::Arc;

use alloy_consensus::{Header, transaction::SignerRecoverable};
use alloy_primitives::{Address, B256};
use base_common_chains::{BaseFeature, Upgrades};
use base_common_consensus::{BaseReceipt, BaseTxEnvelope, DepositReceiptExt};
use base_execution_chainspec::BaseChainSpec;
use base_execution_consensus::{
    calculate_receipt_root_no_memo, validation::validate_body_against_header_base,
};

use crate::{AccountChange, ArchiveError, BlockBody, BlockData, Family};

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

    /// Checks the body against the header with the node's own consensus rule: ommers hash,
    /// transaction root and the withdrawals list (absent before Canyon, present and empty
    /// after, root-checked before Isthmus).
    pub fn verify_body(&self, header: &Header, body: &BlockBody) -> Result<(), ArchiveError> {
        let consensus_body = alloy_consensus::BlockBody::<BaseTxEnvelope> {
            transactions: body.transactions.clone(),
            ommers: Vec::new(),
            withdrawals: body.withdrawals.clone(),
        };
        validate_body_against_header_base(self.chain_spec.as_ref(), &consensus_body, header)
            .map_err(|e| {
                ArchiveError::unavailable(Family::Body, format!("block {}: {e}", header.number))
            })
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

    /// Whether the receipt root at `timestamp` leaves the deposit nonce uncovered (Regolith
    /// active, Canyon not yet).
    pub fn deposit_nonce_uncovered(&self, timestamp: u64) -> bool {
        let spec = self.chain_spec.as_ref();
        spec.is_feature_active_at_timestamp(BaseFeature::DepositNonce, timestamp)
            && !spec.is_feature_active_at_timestamp(BaseFeature::DepositReceiptVersion, timestamp)
    }

    /// In the Regolith window, checks each deposit receipt's nonce, which the receipt root does
    /// not cover: it must equal the sender's nonce before the block (from the account
    /// changeset) plus the number of earlier transactions of the block from the same sender,
    /// since every transaction increments its sender's nonce once. A deposit sender absent from
    /// the changeset is `Unavailable`. Outside the window this is a no-op.
    pub fn verify_deposit_nonces(
        &self,
        header: &Header,
        senders: &[Address],
        receipts: &[BaseReceipt],
        account_changes: &[AccountChange],
    ) -> Result<(), ArchiveError> {
        if !self.deposit_nonce_uncovered(header.timestamp) {
            return Ok(());
        }
        let number = header.number;
        for (index, receipt) in receipts.iter().enumerate() {
            let Some(deposit) = receipt.as_deposit_receipt() else { continue };
            let sender = *senders.get(index).ok_or_else(|| {
                ArchiveError::unavailable(Family::Senders, format!("block {number} lacks sender {index}"))
            })?;
            let before = account_changes.iter().find(|c| c.address == sender).ok_or_else(|| {
                ArchiveError::unavailable(
                    Family::AccountChanges,
                    format!("block {number} deposit {index} sender {sender} is not in the account changeset"),
                )
            })?;
            let earlier = senders[..index].iter().filter(|s| **s == sender).count() as u64;
            let expected = before.before.map_or(0, |a| a.nonce).saturating_add(earlier);
            if deposit.deposit_nonce != Some(expected) {
                return Err(ArchiveError::unavailable(
                    Family::Receipts,
                    format!(
                        "block {number} deposit {index} nonce {:?} differs from sender nonce {expected}",
                        deposit.deposit_nonce
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Checks body, receipts (including Regolith deposit nonces) and senders of one block.
    pub fn verify_block(&self, block: &BlockData) -> Result<(), ArchiveError> {
        self.verify_body(&block.header, &block.body)?;
        self.verify_receipts(&block.header, block.body.transactions.len(), &block.receipts)?;
        self.verify_senders(block.number(), &block.body.transactions, &block.senders)?;
        self.verify_deposit_nonces(&block.header, &block.senders, &block.receipts, &block.account_changes)
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

    /// B1: dropping the empty withdrawals list after Canyon or adding one before Canyon is
    /// rejected, although the transaction root still matches.
    #[test]
    fn withdrawals_presence_follows_canyon() {
        let fixture = ChainFixture::standard();
        let verifier = fixture.verifier();
        let mut post = fixture.blocks[20].clone();
        assert_eq!(post.body.withdrawals, Some(Default::default()), "Canyon block has empty withdrawals");
        post.body.withdrawals = None;
        assert!(
            matches!(verifier.verify_block(&post), Err(ArchiveError::Unavailable { family: Family::Body, .. })),
            "dropped withdrawals after Canyon rejected"
        );
        let mut pre = fixture.blocks[5].clone();
        assert_eq!(pre.body.withdrawals, None, "pre-Canyon block has no withdrawals");
        pre.body.withdrawals = Some(Default::default());
        assert!(
            matches!(verifier.verify_block(&pre), Err(ArchiveError::Unavailable { family: Family::Body, .. })),
            "added withdrawals before Canyon rejected"
        );
    }

    /// B2: a Regolith-window deposit nonce is not covered by the receipt root, so a nonce-only
    /// change passes the root check but fails the changeset check; a deposit sender missing
    /// from the changeset is unavailable. After Canyon the root covers the nonce.
    #[test]
    fn regolith_deposit_nonce_is_checked_against_changeset() {
        let fixture = ChainFixture::standard();
        let verifier = fixture.verifier();
        let block = &fixture.blocks[5];
        assert!(verifier.deposit_nonce_uncovered(block.header.timestamp), "block 5 is in the Regolith window");
        assert!(!verifier.deposit_nonce_uncovered(fixture.blocks[20].header.timestamp), "block 20 is after Canyon");
        let mut corrupt = block.clone();
        corrupt.receipts[0].as_deposit_receipt_mut().expect("deposit").deposit_nonce = Some(99);
        assert_eq!(
            verifier.verify_receipts(&corrupt.header, corrupt.body.transactions.len(), &corrupt.receipts),
            Ok(()),
            "the receipt root does not see the nonce"
        );
        assert!(
            matches!(verifier.verify_block(&corrupt), Err(ArchiveError::Unavailable { family: Family::Receipts, .. })),
            "nonce-only corruption rejected"
        );
        let mut missing = block.clone();
        missing.account_changes.retain(|c| c.address != fixture.depositor);
        assert!(
            matches!(verifier.verify_block(&missing), Err(ArchiveError::Unavailable { family: Family::AccountChanges, .. })),
            "deposit sender missing from the changeset"
        );
        let mut canyon = fixture.blocks[20].clone();
        canyon.receipts[0].as_deposit_receipt_mut().expect("deposit").deposit_nonce = Some(99);
        assert!(
            matches!(verifier.verify_block(&canyon), Err(ArchiveError::Unavailable { family: Family::Receipts, .. })),
            "after Canyon the root covers the nonce"
        );
    }
}
