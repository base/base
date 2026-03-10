use std::collections::VecDeque;

use alloy_consensus::{Header, SignableTransaction};
use alloy_eips::BlockNumHash;
use alloy_primitives::{Address, B256, TxKind, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_alloy_consensus::{OpBlock, OpTxEnvelope};
use base_consensus_genesis::{L1ChainConfig, RollupConfig, SystemConfig};
use base_protocol::{BlockInfo, L1BlockInfoTx, L2BlockInfo};

use crate::{L2BlockProvider, SharedL1Chain};

/// Hardcoded private key for the test account used across all action tests.
///
/// The corresponding address is deterministic: derive it via
/// `PrivateKeySigner::from_bytes(&TEST_ACCOUNT_KEY).unwrap().address()`.
/// Tests that need to fund the account should include it in the genesis
/// allocation with a sufficient ETH balance.
pub const TEST_ACCOUNT_KEY: B256 = B256::new([0x01u8; 32]);

/// The L2 address derived from [`TEST_ACCOUNT_KEY`].
///
/// Pre-computed so callers can reference it without constructing a signer.
// Address derived from the secp256k1 public key of [0x01; 32].
pub const TEST_ACCOUNT_ADDRESS: Address =
    alloy_primitives::address!("1563915e194d8cfba1943570603f7606a3115508");

/// Error type returned by [`L2BlockBuilder`].
#[derive(Debug, thiserror::Error)]
pub enum L2BuilderError {
    /// The L1 block required for the current epoch is missing from the chain.
    #[error("L1 block {0} not found in shared chain")]
    MissingL1Block(u64),
    /// Failed to build the L1 info deposit transaction.
    #[error("failed to build L1 info deposit: {0}")]
    L1Info(#[from] base_protocol::BlockInfoError),
    /// Transaction signing failed.
    #[error("signing failed: {0}")]
    Signing(#[from] alloy_signer::Error),
}

/// A pre-built queue of [`OpBlock`]s for the batcher to drain.
///
/// Tests push fully-formed blocks into the source, which the batcher
/// consumes one at a time via [`L2BlockProvider::next_block`].
#[derive(Debug, Default)]
pub struct ActionL2Source {
    blocks: VecDeque<OpBlock>,
}

impl ActionL2Source {
    /// Create an empty source.
    pub const fn new() -> Self {
        Self { blocks: VecDeque::new() }
    }

    /// Push a block to the back of the queue.
    pub fn push(&mut self, block: OpBlock) {
        self.blocks.push_back(block);
    }

    /// Return the number of blocks remaining.
    pub fn remaining(&self) -> usize {
        self.blocks.len()
    }

    /// Return `true` if the source has been fully drained.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

impl L2BlockProvider for ActionL2Source {
    fn next_block(&mut self) -> Option<OpBlock> {
        self.blocks.pop_front()
    }
}

/// Builds real [`OpBlock`]s for use in action tests.
///
/// Each block contains:
/// - A correct L1-info deposit transaction (type `0x7E`) as the first
///   transaction, built from the actual L1 block at the current epoch.
/// - A configurable number of signed EIP-1559 user transactions from the
///   test account ([`TEST_ACCOUNT_KEY`]).
///
/// Epoch selection mirrors the real sequencer: the epoch advances to the next
/// L1 block once that block's timestamp is ≤ the new L2 block's timestamp.
///
/// # State root
///
/// `Header::state_root` is set to `B256::ZERO`. Real state-root computation
/// (via `PendingStateBuilder`) is not yet wired in.
///
/// TODO: integrate `PendingStateBuilder` (crates/execution/flashblocks) so
/// that each block is executed against an `InMemoryDB` seeded with the test
/// account balance and the `L1Block` precompile state.
#[derive(Debug)]
pub struct L2BlockBuilder {
    /// Current unsafe L2 head.
    head: L2BlockInfo,
    /// Shared view of the L1 chain for epoch selection.
    l1_chain: SharedL1Chain,
    /// Rollup configuration.
    rollup_config: RollupConfig,
    /// L1 chain config (needed for [`L1BlockInfoTx`]).
    l1_chain_config: L1ChainConfig,
    /// Current system config (updated on epoch changes or key rotation).
    system_config: SystemConfig,
    /// Signer for user transactions.
    signer: PrivateKeySigner,
    /// Number of user EIP-1559 transactions to include per block.
    user_txs_per_block: usize,
    /// Per-account nonce for the test signer.
    nonce: u64,
}

impl L2BlockBuilder {
    /// Create a new builder starting from the given L2 genesis head.
    pub fn new(
        head: L2BlockInfo,
        l1_chain: SharedL1Chain,
        rollup_config: RollupConfig,
        system_config: SystemConfig,
    ) -> Self {
        let signer = PrivateKeySigner::from_bytes(&TEST_ACCOUNT_KEY)
            .expect("TEST_ACCOUNT_KEY is a valid secp256k1 scalar");
        Self {
            head,
            l1_chain,
            rollup_config,
            l1_chain_config: L1ChainConfig::default(),
            system_config,
            signer,
            user_txs_per_block: 1,
            nonce: 0,
        }
    }

    /// Set the number of signed user transactions included per block.
    pub const fn with_user_txs_per_block(mut self, n: usize) -> Self {
        self.user_txs_per_block = n;
        self
    }

    /// Return the current unsafe L2 head.
    pub const fn head(&self) -> L2BlockInfo {
        self.head
    }

    /// Build the next L2 block and advance the internal head.
    ///
    /// Returns a fully-formed [`OpBlock`] containing the L1-info deposit and
    /// any configured user transactions.
    pub fn build_next_block(&mut self) -> Result<OpBlock, L2BuilderError> {
        let next_number = self.head.block_info.number + 1;
        let next_timestamp = self.head.block_info.timestamp + self.rollup_config.block_time;
        let parent_hash = self.head.block_info.hash;
        let current_epoch = self.head.l1_origin.number;

        // Epoch selection: advance when the next L1 block's timestamp is reached.
        let (epoch_number, l1_header) = {
            if let Some(next_l1) = self.l1_chain.get_block(current_epoch + 1) {
                if next_l1.timestamp() <= next_timestamp {
                    let hdr = next_l1.header.clone();
                    (next_l1.number(), hdr)
                } else {
                    let cur = self
                        .l1_chain
                        .get_block(current_epoch)
                        .ok_or(L2BuilderError::MissingL1Block(current_epoch))?;
                    let hdr = cur.header.clone();
                    (cur.number(), hdr)
                }
            } else {
                let cur = self
                    .l1_chain
                    .get_block(current_epoch)
                    .ok_or(L2BuilderError::MissingL1Block(current_epoch))?;
                let hdr = cur.header.clone();
                (cur.number(), hdr)
            }
        };

        let seq_num =
            if epoch_number == self.head.l1_origin.number { self.head.seq_num + 1 } else { 0 };

        // Build the L1 info deposit (first transaction in every L2 block).
        let (_l1_info, deposit_tx) = L1BlockInfoTx::try_new_with_deposit_tx(
            &self.rollup_config,
            &self.l1_chain_config,
            &self.system_config,
            seq_num,
            &l1_header,
            next_timestamp,
        )?;

        let mut transactions: Vec<OpTxEnvelope> = vec![OpTxEnvelope::Deposit(deposit_tx)];

        // Build signed EIP-1559 user transactions.
        for _ in 0..self.user_txs_per_block {
            let tx = alloy_consensus::TxEip1559 {
                chain_id: self.rollup_config.l2_chain_id.id(),
                nonce: self.nonce,
                max_fee_per_gas: 1_000_000_000,
                max_priority_fee_per_gas: 1_000_000,
                gas_limit: 21_000,
                to: TxKind::Call(Address::ZERO),
                value: U256::from(1u64),
                input: alloy_primitives::Bytes::new(),
                access_list: Default::default(),
            };
            let sig = self.signer.sign_hash_sync(&tx.signature_hash())?;
            self.nonce += 1;
            transactions.push(OpTxEnvelope::Eip1559(tx.into_signed(sig)));
        }

        let epoch_hash = l1_header.hash_slow();
        let header = Header {
            number: next_number,
            timestamp: next_timestamp,
            parent_hash,
            gas_limit: 30_000_000,
            // State root is not computed (no EVM execution yet).
            // TODO: integrate PendingStateBuilder for real state root computation.
            ..Default::default()
        };

        let block = OpBlock {
            header,
            body: alloy_consensus::BlockBody { transactions, ommers: vec![], withdrawals: None },
        };

        self.head = L2BlockInfo {
            block_info: BlockInfo {
                number: next_number,
                timestamp: next_timestamp,
                parent_hash,
                // Hash is left as zero — the block hash would require sealing.
                // TODO: seal the header and use the real hash.
                hash: B256::ZERO,
            },
            l1_origin: BlockNumHash { number: epoch_number, hash: epoch_hash },
            seq_num,
        };

        Ok(block)
    }
}

impl L2BlockProvider for L2BlockBuilder {
    fn next_block(&mut self) -> Option<OpBlock> {
        self.build_next_block().ok()
    }
}
