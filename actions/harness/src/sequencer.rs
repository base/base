use std::collections::VecDeque;

use alloy_consensus::{BlockBody, Header};
use alloy_eips::BlockNumHash;
use alloy_primitives::B256;
use base_alloy_consensus::{OpBlock, OpTxEnvelope};
use base_consensus_genesis::{L1ChainConfig, RollupConfig, SystemConfig};
use base_protocol::{BlockInfo, L1BlockInfoTx, L2BlockInfo};

use crate::{Action, L2BlockProvider, SharedL1Chain};

/// Errors returned by [`L2Sequencer`].
#[derive(Debug, thiserror::Error)]
pub enum SequencerError {
    /// The L1 block required to determine the current epoch is missing.
    #[error("L1 block {0} not found in shared chain")]
    MissingL1Block(u64),
    /// Failed to build the L1 info deposit transaction.
    #[error("failed to build L1 info deposit: {0}")]
    L1Info(#[from] base_protocol::BlockInfoError),
}

/// In-memory L2 sequencer actor for action tests.
///
/// `L2Sequencer` builds [`OpBlock`]s anchored to the L1 chain and advances
/// the "unsafe" L2 head. It implements [`L2BlockProvider`] so it can be
/// passed directly to [`Batcher`] for batch encoding.
///
/// Each call to [`act_l2_start_block`] (or [`Action::act`]) produces one L2
/// block, enqueuing it in an internal queue. The batcher drains this queue
/// via [`L2BlockProvider::next_block`].
///
/// Each block contains a correct L1-info deposit transaction (type `0x7E`) as
/// its first transaction, built from the epoch's L1 block. No user
/// transactions are included and no EVM execution is performed — state roots
/// are `B256::ZERO`. This is sufficient for testing epoch selection, sequencer
/// number/timestamp logic, and end-to-end derivation without the overhead of
/// full block execution.
///
/// # Epoch selection
///
/// The sequencer advances its L1 epoch (origin) when the next L2 block's
/// timestamp is ≥ the next L1 block's timestamp. Otherwise it stays on the
/// current epoch. This mirrors the real sequencer's epoch-advance rule.
///
/// [`act_l2_start_block`]: L2Sequencer::act_l2_start_block
/// [`Batcher`]: crate::Batcher
#[derive(Debug)]
pub struct L2Sequencer {
    /// Current unsafe L2 head (the tip the sequencer is building on top of).
    unsafe_head: L2BlockInfo,
    /// Shared view of the L1 chain, used for epoch selection.
    l1_chain: SharedL1Chain,
    /// Rollup configuration (provides `block_time` and hardfork activation).
    rollup_config: RollupConfig,
    /// L1 chain config, needed to build `L1BlockInfoTx`.
    l1_chain_config: L1ChainConfig,
    /// System config, needed to build `L1BlockInfoTx`.
    system_config: SystemConfig,
    /// Queue of built L2 blocks, available for the batcher to drain.
    built: VecDeque<OpBlock>,
}

impl L2Sequencer {
    /// Create a new [`L2Sequencer`] starting from the given unsafe head.
    ///
    /// `unsafe_head` is typically the L2 genesis block. Pass the same
    /// [`SharedL1Chain`] that the verifier's providers use so both actors
    /// see a consistent L1 view; when new L1 blocks are pushed to the chain
    /// the sequencer will pick up the updated epoch information automatically.
    pub fn new(
        unsafe_head: L2BlockInfo,
        l1_chain: SharedL1Chain,
        rollup_config: RollupConfig,
    ) -> Self {
        Self {
            unsafe_head,
            l1_chain,
            rollup_config,
            l1_chain_config: L1ChainConfig::default(),
            system_config: SystemConfig::default(),
            built: VecDeque::new(),
        }
    }

    /// Return the current unsafe L2 head.
    pub const fn unsafe_head(&self) -> L2BlockInfo {
        self.unsafe_head
    }

    /// Return the number of built but not-yet-consumed L2 blocks in the queue.
    pub fn queued_blocks(&self) -> usize {
        self.built.len()
    }

    /// Build one L2 block and enqueue it for the batcher.
    ///
    /// Computes the next block number, timestamp, and L1 epoch (origin), builds
    /// the L1-info deposit transaction, and assembles an [`OpBlock`] containing
    /// only that deposit (no user transactions, no EVM execution). The block is
    /// appended to the internal queue and the unsafe head is advanced.
    ///
    /// Epoch selection:
    /// - If the next L1 block exists in the shared chain **and** its timestamp
    ///   is ≤ the new L2 block's timestamp, the epoch advances to that L1 block.
    /// - Otherwise the current epoch is retained.
    ///
    /// # Errors
    ///
    /// Returns [`SequencerError::MissingL1Block`] if the current epoch's L1
    /// block is absent from the shared chain (which should not happen in a
    /// correctly initialised test).
    ///
    /// Returns [`SequencerError::L1Info`] if the L1-info deposit transaction
    /// cannot be constructed from the epoch header.
    pub fn act_l2_start_block(&mut self) -> Result<(), SequencerError> {
        let next_number = self.unsafe_head.block_info.number + 1;
        let next_timestamp = self.unsafe_head.block_info.timestamp + self.rollup_config.block_time;
        let parent_hash = self.unsafe_head.block_info.hash;
        let current_epoch = self.unsafe_head.l1_origin.number;

        let (epoch_number, l1_header) = {
            if let Some(next_l1) = self.l1_chain.get_block(current_epoch + 1) {
                if next_l1.timestamp() <= next_timestamp {
                    let hdr = next_l1.header.clone();
                    (next_l1.number(), hdr)
                } else {
                    let cur = self
                        .l1_chain
                        .get_block(current_epoch)
                        .ok_or(SequencerError::MissingL1Block(current_epoch))?;
                    let hdr = cur.header.clone();
                    (cur.number(), hdr)
                }
            } else {
                let cur = self
                    .l1_chain
                    .get_block(current_epoch)
                    .ok_or(SequencerError::MissingL1Block(current_epoch))?;
                let hdr = cur.header.clone();
                (cur.number(), hdr)
            }
        };

        let seq_num = if epoch_number == self.unsafe_head.l1_origin.number {
            self.unsafe_head.seq_num + 1
        } else {
            0
        };

        let (_l1_info, deposit_tx) = L1BlockInfoTx::try_new_with_deposit_tx(
            &self.rollup_config,
            &self.l1_chain_config,
            &self.system_config,
            seq_num,
            &l1_header,
            next_timestamp,
        )?;

        let epoch_hash = l1_header.hash_slow();

        let block = OpBlock {
            header: Header {
                number: next_number,
                timestamp: next_timestamp,
                parent_hash,
                state_root: B256::ZERO,
                gas_limit: 30_000_000,
                ..Default::default()
            },
            body: BlockBody {
                transactions: vec![OpTxEnvelope::Deposit(deposit_tx)],
                ommers: vec![],
                withdrawals: None,
            },
        };

        self.built.push_back(block);

        self.unsafe_head = L2BlockInfo {
            block_info: BlockInfo {
                number: next_number,
                timestamp: next_timestamp,
                parent_hash,
                ..Default::default()
            },
            l1_origin: BlockNumHash { number: epoch_number, hash: epoch_hash },
            seq_num,
        };

        Ok(())
    }
}

impl L2BlockProvider for L2Sequencer {
    fn next_block(&mut self) -> Option<OpBlock> {
        self.built.pop_front()
    }
}

impl Action for L2Sequencer {
    type Output = ();
    type Error = SequencerError;

    fn act(&mut self) -> Result<(), SequencerError> {
        self.act_l2_start_block()
    }
}
