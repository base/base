use alloc::{sync::Arc, vec::Vec};

use base_common_types_chain::{BaseReceipt, RecoveredBlock};

use crate::{BlockReceipts, Chain};

/// A notification that is sent when a new block is imported, or an old block is reverted.
///
/// The notification contains at least one [`Chain`] with the imported segment. If some blocks were
/// reverted (e.g. during a reorg), the old chain is also returned.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(bound = ""))]
pub enum CanonStateNotification {
    /// The canonical chain was extended.
    Commit {
        /// The newly added chain segment.
        new: Arc<Chain>,
    },
    /// A chain segment was reverted or reorged.
    ///
    /// - In the case of a reorg, the reverted blocks are present in `old`, and the new blocks are
    ///   present in `new`.
    /// - In the case of a revert, the reverted blocks are present in `old`, and `new` is an empty
    ///   chain segment.
    Reorg {
        /// The chain segment that was reverted.
        old: Arc<Chain>,
        /// The chain segment that was added on top of the canonical chain, minus the reverted
        /// blocks.
        ///
        /// In the case of a revert, not a reorg, this chain segment is empty.
        new: Arc<Chain>,
    },
}

impl CanonStateNotification {
    /// Get the chain segment that was reverted, if any.
    pub fn reverted(&self) -> Option<Arc<Chain>> {
        match self {
            Self::Commit { .. } => None,
            Self::Reorg { old, .. } => Some(old.clone()),
        }
    }

    /// Get the newly imported chain segment, if any.
    pub fn committed(&self) -> Arc<Chain> {
        match self {
            Self::Commit { new } | Self::Reorg { new, .. } => new.clone(),
        }
    }

    /// Gets the new tip of the chain.
    ///
    /// Returns the new tip for [`Self::Reorg`] and [`Self::Commit`] variants which commit at least
    /// 1 new block.
    ///
    /// # Panics
    ///
    /// If chain doesn't have any blocks.
    pub fn tip(&self) -> &RecoveredBlock {
        match self {
            Self::Commit { new } | Self::Reorg { new, .. } => new.tip(),
        }
    }

    /// Gets the new tip of the chain.
    ///
    /// If the chain has no blocks, it returns `None`. Otherwise, it returns the new tip for
    /// [`Self::Reorg`] and [`Self::Commit`] variants.
    pub fn tip_checked(&self) -> Option<&RecoveredBlock> {
        match self {
            Self::Commit { new } | Self::Reorg { new, .. } => {
                if new.is_empty() {
                    None
                } else {
                    Some(new.tip())
                }
            }
        }
    }

    /// Get receipts in the reverted and newly imported chain segments with their corresponding
    /// block numbers and transaction hashes.
    ///
    /// The boolean in the tuple (2nd element) denotes whether the receipt was from the reverted
    /// chain segment.
    pub fn block_receipts(&self) -> Vec<(BlockReceipts<BaseReceipt>, bool)> {
        let mut receipts = Vec::new();

        // get old receipts
        if let Some(old) = self.reverted() {
            receipts
                .extend(old.receipts_with_attachment().into_iter().map(|receipt| (receipt, true)));
        }
        // get new receipts
        receipts.extend(
            self.committed().receipts_with_attachment().into_iter().map(|receipt| (receipt, false)),
        );
        receipts
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use alloy_primitives::{B256, Signature, b256};
    use base_common_types_chain::{
        BaseTxEnvelope as TransactionSigned, BlockBody, SealedBlock, SealedHeader,
        SignableTransaction, TxLegacy,
    };

    use super::*;
    use crate::ExecutionOutcome;

    #[test]
    fn test_commit_notification() {
        let block: RecoveredBlock = Default::default();
        let block1_hash = B256::new([0x01; 32]);
        let block2_hash = B256::new([0x02; 32]);

        let mut block1 = block.clone();
        block1.set_block_number(1);
        block1.set_hash(block1_hash);

        let mut block2 = block;
        block2.set_block_number(2);
        block2.set_hash(block2_hash);

        let chain: Arc<Chain> = Arc::new(Chain::new(
            vec![block1.clone(), block2.clone()],
            ExecutionOutcome::default(),
            BTreeMap::new(),
        ));

        // Create a commit notification
        let notification = CanonStateNotification::Commit { new: chain.clone() };

        // Test that `committed` returns the correct chain
        assert_eq!(notification.committed(), chain);

        // Test that `reverted` returns None for `Commit`
        assert!(notification.reverted().is_none());

        // Test that `tip` returns the correct block
        assert_eq!(*notification.tip(), block2);
    }

    #[test]
    fn test_reorg_notification() {
        let block: RecoveredBlock = Default::default();
        let block1_hash = B256::new([0x01; 32]);
        let block2_hash = B256::new([0x02; 32]);
        let block3_hash = B256::new([0x03; 32]);

        let mut block1 = block.clone();
        block1.set_block_number(1);
        block1.set_hash(block1_hash);

        let mut block2 = block.clone();
        block2.set_block_number(2);
        block2.set_hash(block2_hash);

        let mut block3 = block;
        block3.set_block_number(3);
        block3.set_hash(block3_hash);

        let old_chain: Arc<Chain> = Arc::new(Chain::new(
            vec![block1.clone()],
            ExecutionOutcome::default(),
            BTreeMap::new(),
        ));
        let new_chain = Arc::new(Chain::new(
            vec![block2.clone(), block3.clone()],
            ExecutionOutcome::default(),
            BTreeMap::new(),
        ));

        // Create a reorg notification
        let notification =
            CanonStateNotification::Reorg { old: old_chain.clone(), new: new_chain.clone() };

        // Test that `reverted` returns the old chain
        assert_eq!(notification.reverted(), Some(old_chain));

        // Test that `committed` returns the new chain
        assert_eq!(notification.committed(), new_chain);

        // Test that `tip` returns the tip of the new chain (last block in the new chain)
        assert_eq!(*notification.tip(), block3);
    }

    #[test]
    fn test_block_receipts_commit() {
        // Create a default block instance for use in block definitions.
        let mut body = BlockBody::<TransactionSigned>::default();

        // Define unique hashes for two blocks to differentiate them in the chain.
        let block1_hash = B256::new([0x01; 32]);
        let block2_hash = B256::new([0x02; 32]);

        // Create a default transaction to include in block1's transactions.
        let tx = TxLegacy::default().into_signed(Signature::test_signature()).into();
        body.transactions.push(tx);

        let block = SealedBlock::from_sealed_parts(
            SealedHeader::seal_slow(base_common_types_chain::Header::default()),
            body,
        )
        .try_recover()
        .unwrap();

        // Create a clone of the default block and customize it to act as block1.
        let mut block1 = block.clone();
        block1.set_block_number(1);
        block1.set_hash(block1_hash);

        // Clone the default block and customize it to act as block2.
        let mut block2 = block;
        block2.set_block_number(2);
        block2.set_hash(block2_hash);

        // Create a receipt for the transaction in block1.
        let receipt1 = BaseReceipt::Legacy(base_common_types_chain::Receipt {
            cumulative_gas_used: 12345,
            logs: vec![],
            status: true.into(),
        });

        // Wrap the receipt in a `Receipts` structure, as expected in the `ExecutionOutcome`.
        let receipts = vec![vec![receipt1.clone()]];

        // Define an `ExecutionOutcome` with the created receipts.
        let execution_outcome = ExecutionOutcome { receipts, ..Default::default() };

        // Create a new chain segment with `block1` and `block2` and the execution outcome.
        let new_chain: Arc<Chain> = Arc::new(Chain::new(
            vec![block1.clone(), block2.clone()],
            execution_outcome,
            BTreeMap::new(),
        ));

        // Create a commit notification containing the new chain segment.
        let notification = CanonStateNotification::Commit { new: new_chain };

        // Call `block_receipts` on the commit notification to retrieve block receipts.
        let block_receipts = notification.block_receipts();

        // Assert that only one receipt entry exists in the `block_receipts` list.
        assert_eq!(block_receipts.len(), 1);

        // Verify that the first entry matches block1's hash and transaction receipt.
        assert_eq!(
            block_receipts[0].0,
            BlockReceipts {
                block: block1.num_hash(),
                timestamp: block1.timestamp,
                tx_receipts: vec![(
                    // Transaction hash of a Transaction::default()
                    b256!("0x20b5378c6fe992c118b557d2f8e8bbe0b7567f6fe5483a8f0f1c51e93a9d91ab"),
                    receipt1
                )]
            }
        );

        // Assert that the receipt is from the committed segment (not reverted).
        assert!(!block_receipts[0].1);
    }

    #[test]
    fn test_block_receipts_reorg() {
        // Define block1 for the old chain segment, which will be reverted.
        let mut body = BlockBody::<TransactionSigned>::default();
        body.transactions.push(TxLegacy::default().into_signed(Signature::test_signature()).into());
        let mut old_block1 = SealedBlock::from_sealed_parts(
            SealedHeader::seal_slow(base_common_types_chain::Header::default()),
            body,
        )
        .try_recover()
        .unwrap();
        old_block1.set_block_number(1);
        old_block1.set_hash(B256::new([0x01; 32]));

        // Create a receipt for a transaction in the reverted block.
        let old_receipt = BaseReceipt::Legacy(base_common_types_chain::Receipt {
            cumulative_gas_used: 54321,
            logs: vec![],
            status: false.into(),
        });
        let old_receipts = vec![vec![old_receipt.clone()]];

        let old_execution_outcome =
            ExecutionOutcome { receipts: old_receipts, ..Default::default() };

        // Create an old chain segment to be reverted, containing `old_block1`.
        let old_chain: Arc<Chain> =
            Arc::new(Chain::new(vec![old_block1.clone()], old_execution_outcome, BTreeMap::new()));

        // Define block2 for the new chain segment, which will be committed.
        let mut body = BlockBody::<TransactionSigned>::default();
        body.transactions.push(TxLegacy::default().into_signed(Signature::test_signature()).into());
        let mut new_block1 = SealedBlock::from_sealed_parts(
            SealedHeader::seal_slow(base_common_types_chain::Header::default()),
            body,
        )
        .try_recover()
        .unwrap();
        new_block1.set_block_number(2);
        new_block1.set_hash(B256::new([0x02; 32]));

        // Create a receipt for a transaction in the new committed block.
        let new_receipt = BaseReceipt::Legacy(base_common_types_chain::Receipt {
            cumulative_gas_used: 12345,
            logs: vec![],
            status: true.into(),
        });
        let new_receipts = vec![vec![new_receipt.clone()]];

        let new_execution_outcome =
            ExecutionOutcome { receipts: new_receipts, ..Default::default() };

        // Create a new chain segment to be committed, containing `new_block1`.
        let new_chain =
            Arc::new(Chain::new(vec![new_block1.clone()], new_execution_outcome, BTreeMap::new()));

        // Create a reorg notification with both reverted (old) and committed (new) chain segments.
        let notification = CanonStateNotification::Reorg { old: old_chain, new: new_chain };

        // Retrieve receipts from both old (reverted) and new (committed) segments.
        let block_receipts = notification.block_receipts();

        // Assert there are two receipt entries, one from each chain segment.
        assert_eq!(block_receipts.len(), 2);

        // Verify that the first entry matches old_block1 and its receipt from the reverted segment.
        assert_eq!(
            block_receipts[0].0,
            BlockReceipts {
                block: old_block1.num_hash(),
                timestamp: old_block1.timestamp,
                tx_receipts: vec![(
                    // Transaction hash of a Transaction::default()
                    b256!("0x20b5378c6fe992c118b557d2f8e8bbe0b7567f6fe5483a8f0f1c51e93a9d91ab"),
                    old_receipt
                )]
            }
        );
        // Confirm this is from the reverted segment.
        assert!(block_receipts[0].1);

        // Verify that the second entry matches new_block1 and its receipt from the committed
        // segment.
        assert_eq!(
            block_receipts[1].0,
            BlockReceipts {
                block: new_block1.num_hash(),
                timestamp: new_block1.timestamp,
                tx_receipts: vec![(
                    // Transaction hash of a Transaction::default()
                    b256!("0x20b5378c6fe992c118b557d2f8e8bbe0b7567f6fe5483a8f0f1c51e93a9d91ab"),
                    new_receipt
                )]
            }
        );
        // Confirm this is from the committed segment.
        assert!(!block_receipts[1].1);
    }
}
